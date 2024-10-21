use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::{atomic::AtomicUsize, Arc, Mutex},
};

use candle_core::{
    cuda::{
        cudarc::{
            cublas::{result::hgemm, sys::cublasOperation_t},
            driver::{CudaSlice, DevicePtr},
        },
        CudaStorageSlice, WrapErr,
    },
    from_storage_no_op, Context, CudaStorage, DType, Device, Result, Shape, Storage, Tensor, D,
};
use candle_nn::VarBuilder;
use half::f16;
use lazy_static::lazy_static;

#[cfg(feature = "marlin")]
use crate::gptq::marlin_backend::{gptq_marlin_matmul, gptq_weight_repack};


use crate::{
    utils::{get_cuda_device, get_cuda_slice},
    DummyLayer, IsqType, QuantMethod, QuantMethodConfig, QuantizedConfig, QuantizedSerde,
};

use super::ffi::{
    gemm_half_q_half_alt, gemm_half_q_half_cuda_part, reconstruct_exllama, reconstruct_gptq,
};

const MAX_Q_GEMM_ROWS_8BIT: i32 = 24;
const MAX_Q_GEMM_ROWS: i32 = 50;
const MAX_ALT_GEMM_ROWS: i32 = 8;
const BLOCK_M_SIZE_MAX: i32 = 8;

lazy_static! {
    static ref TMP_DQS: Mutex<HashMap<usize, CudaSlice<f16>>> = Mutex::new(HashMap::new());
}

#[derive(Debug)]
pub struct GptqLayer {
    q_weight: Tensor,            // u32
    gptq_qzeros: Option<Tensor>, // u32
    gptq_scales: Tensor,         // f16
    bias: Option<Tensor>,        // f16
    g_idx: Option<Tensor>,       // i32
    bits: i32,
    use_exllama: bool,
    workspace: Option<Tensor>,
    is_marlin: bool,
}

impl GptqLayer {
    // https://github.com/vllm-project/vllm/blob/966fe72141e8365721840b7ababfb78601c23ead/csrc/quantization/gptq/q_gemm.cu#L1490
    // https://github.com/vllm-project/vllm/blob/966fe72141e8365721840b7ababfb78601c23ead/csrc/quantization/gptq/q_gemm.cu#L1823
    fn gptq_gemm(
        &self,
        a: Tensor,
        g_idx: &Tensor,
        gptq_qzeros: &Tensor,
        groups: i32,
        use_exllama: bool,
    ) -> Result<Tensor> {
        if !a.is_contiguous() {
            candle_core::bail!(
                "Expected `a` to be contiguous, got strides {:?}",
                a.layout().stride()
            )
        }
        let a_ptr = get_cuda_slice::<f16>(&a)?;
        let b_q_weight = get_cuda_slice::<i32>(&self.q_weight)? as *const u32;
        let b_gptq_qzeros = get_cuda_slice::<i32>(gptq_qzeros)? as *const u32;
        let b_gptq_scales = get_cuda_slice::<f16>(&self.gptq_scales)?;
        let b_g_idx = get_cuda_slice::<i32>(g_idx)?;

        let dev = get_cuda_device(&a)?;

        let c_shape = Shape::from_dims(&[a.dims()[0], self.q_weight.dims()[1]]);

        let (m, n, k) = (
            c_shape.dims()[0] as i32,
            c_shape.dims()[1] as i32,
            a.dims()[1] as i32,
        );

        let c = unsafe { dev.alloc::<f16>(c_shape.elem_count()).w()? };

        let c_ptr = *c.device_ptr() as *mut f16;

        let len = (self.q_weight.dims()[0] * 32 / self.bits as usize) * self.q_weight.dims()[1];
        let temp_dq_ptr = *TMP_DQS.try_lock().unwrap().get(&len).unwrap().device_ptr() as *mut f16;

        let use_reconstruct = if use_exllama {
            (self.bits == 8 && m > MAX_Q_GEMM_ROWS_8BIT) || (self.bits != 8 && m > MAX_Q_GEMM_ROWS)
        } else {
            // The 2/3-bit kernels are somehow slower than dequant + gemm baseline, so
            // we disabled them for now.
            self.bits < 4 || m > MAX_ALT_GEMM_ROWS
        };

        if use_reconstruct {
            // Reconstruct FP16 matrix, then cuBLAS

            let cublas_handle = match a.device() {
                Device::Cuda(dev) => dev.cublas_handle(),
                _ => unreachable!(), // invariant enforced earlier
            };

            let reconstruct_kernel = if use_exllama {
                reconstruct_exllama
            } else {
                reconstruct_gptq
            };
            unsafe {
                reconstruct_kernel(
                    b_q_weight,
                    b_gptq_qzeros,
                    b_gptq_scales,
                    b_g_idx,
                    temp_dq_ptr,
                    k,
                    n,
                    groups,
                    self.bits,
                )
            };

            let alpha = f16::from_f32_const(1.0);
            let beta = f16::from_f32_const(0.0);

            unsafe {
                hgemm(
                    *cublas_handle.handle(),
                    cublasOperation_t::CUBLAS_OP_N,
                    cublasOperation_t::CUBLAS_OP_N,
                    n,
                    m,
                    k,
                    &alpha,
                    temp_dq_ptr as *const _,
                    n,
                    a_ptr as *const _,
                    k,
                    &beta,
                    c_ptr,
                    n,
                )
                .w()?
            };
        } else if use_exllama {
            let max_chunks = m / BLOCK_M_SIZE_MAX;
            let last_chunk = max_chunks * BLOCK_M_SIZE_MAX;
            let last_chunk_size = m - last_chunk;

            if max_chunks > 0 {
                unsafe {
                    gemm_half_q_half_cuda_part(
                        a_ptr as *const _,
                        b_q_weight,
                        b_gptq_qzeros,
                        b_gptq_scales,
                        b_g_idx,
                        c_ptr,
                        last_chunk,
                        n,
                        k,
                        BLOCK_M_SIZE_MAX,
                        groups,
                        self.bits,
                    )
                }
            }
            if last_chunk_size > 0 {
                unsafe {
                    gemm_half_q_half_cuda_part(
                        a_ptr.add((last_chunk * k) as usize),
                        b_q_weight,
                        b_gptq_qzeros,
                        b_gptq_scales,
                        b_g_idx,
                        c_ptr.add((last_chunk * n) as usize),
                        last_chunk_size,
                        n,
                        k,
                        last_chunk_size,
                        groups,
                        self.bits,
                    )
                }
            }
        } else {
            unsafe {
                gemm_half_q_half_alt(
                    a_ptr as *const _,
                    b_q_weight,
                    b_gptq_qzeros,
                    b_gptq_scales,
                    b_g_idx,
                    c_ptr,
                    m,
                    n,
                    k,
                    self.bits,
                )
            }
        }

        let storage = CudaStorage {
            slice: CudaStorageSlice::F16(c),
            device: dev.clone(),
        };
        let storage = Storage::Cuda(storage);

        Ok(from_storage_no_op(storage, c_shape, false))
    }
}

impl QuantMethod for GptqLayer {
    fn new(method: QuantMethodConfig) -> Result<Self>
    where
        Self: Sized,
    {
        match method {
            QuantMethodConfig::Gptq {
                bits,
                use_exllama,
                q_weight,
                gptq_qzeros,
                gptq_scales,
                g_idx,
                bias,
                workspace,
                is_marlin,
            } => {
                if workspace.is_none() {
                    let dev = get_cuda_device(&q_weight)?;
                    let len = (q_weight.dims()[0] * 32 / bits as usize) * q_weight.dims()[1];
                    // SAFETY: used in the kernel as a tmp space, just preallocating it here.
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        TMP_DQS.lock().unwrap().entry(len)
                    {
                        e.insert(unsafe { dev.alloc::<f16>(len).w()? });
                    }
                }
                Ok(Self {
                    q_weight,
                    gptq_qzeros,
                    gptq_scales,
                    g_idx,
                    bits,
                    use_exllama,
                    bias,
                    workspace,
                    is_marlin,
                })
            }
            QuantMethodConfig::Gguf { .. }
            | QuantMethodConfig::Unquantized(_)
            | QuantMethodConfig::Hqq { .. }
            | QuantMethodConfig::Dummy
            | QuantMethodConfig::FP8 { .. } => {
                unreachable!()
            }
        }
    }

    fn forward(&self, a: &Tensor) -> Result<Tensor> {
        // https://github.com/vllm-project/vllm/blob/ba991d5c84adbc0685075af88333c688ddb06011/vllm/model_executor/layers/quantization/gptq.py#L200
        let out_shape = Shape::from_dims(
            &[
                &a.dims()[..a.dims().len() - 1],
                &[self.q_weight.dim(D::Minus1)?],
            ]
            .concat(),
        );
        let reshaped_a = a.reshape(((), a.dim(D::Minus1)?))?;
        if !reshaped_a.device().is_cuda() {
            candle_core::bail!("Expected CUDA input to GptqLayer");
        }

        let out = match (
            self.g_idx.as_ref(),
            self.gptq_qzeros.as_ref(),
            self.is_marlin,
        ) {
            (Some(g_idx), Some(gptq_qzeros), false) => self
                .gptq_gemm(
                    reshaped_a,
                    g_idx,
                    gptq_qzeros,
                    gptq_qzeros.dim(0)? as i32,
                    self.use_exllama,
                )?
                .reshape(out_shape)?,
            #[cfg(feature = "marlin")]
            (_, _, true) => gptq_marlin_matmul(
                a,
                &self.q_weight,
                &self.gptq_scales,
                self.workspace.as_ref().context("Workspace required")?,
                self.bits,
            )?,
            _ => unreachable!(),
        };

        if let Some(bias) = &self.bias {
            out.broadcast_add(bias)
        } else {
            Ok(out)
        }
    }

    fn quantized_act_type(&self) -> Option<DType> {
        Some(DType::F16)
    }

    fn add_delta_w(&self, _delta: &Tensor) -> Result<Arc<dyn QuantMethod>> {
        candle_core::bail!("GPTQ quantization does not support adding weight delta.")
    }

    fn dtype_and_device(&self) -> (DType, Device) {
        (self.gptq_scales.dtype(), self.gptq_scales.device().clone())
    }

    fn get_bias_mut(&mut self) -> Option<&mut Tensor> {
        None
    }

    fn apply_isq(
        self: Arc<Self>,
        _dtype: Option<IsqType>,
        _device: Device,
        _n_quantized: &AtomicUsize,
    ) -> Result<Arc<dyn QuantMethod>> {
        candle_core::bail!("GPTQ quantization does not support ISQ.")
    }

    fn get_max_isq_cpu_threads(&self, _dtype: IsqType) -> Option<NonZeroUsize> {
        None
    }
}

impl QuantizedSerde for GptqLayer {
    fn name(&self) -> &'static str {
        "gptq"
    }
}

macro_rules! pack_factor {
    ($bits:expr) => {
        32 / $bits
    };
}
pub fn gptq_linear(
    in_dim: usize,
    out_dim: usize,
    config: &QuantizedConfig,
    vb: VarBuilder,
) -> Result<Arc<dyn QuantMethod>> {
    if !(vb.contains_tensor("qweight")
        && vb.contains_tensor("qzeros")
        && vb.contains_tensor("g_idx")
        && vb.contains_tensor("scales"))
    {
        return Ok(Arc::new(<DummyLayer as QuantMethod>::new(QuantMethodConfig::Dummy)?));
    }

  
    let marlin_compatible = config.bits == 4 || config.bits == 8;
 
    let marlin_format = config
        .checkpoint_format
        .as_ref()
        .is_some_and(|fmt| fmt == "marlin");

    let qw_shape = if cfg!(feature = "marlin") && marlin_format {
        (in_dim / pack_factor!(config.bits) / 2, out_dim * 2)
    } else {
        (in_dim / pack_factor!(config.bits), out_dim)
    };

    let qweight = vb.get_with_hints_dtype(
        qw_shape,
        if cfg!(feature = "marlin") && marlin_format { "B" } else { "qweight" },
        Default::default(),
        DType::I32,
    )?;

    let scale_and_zero_size = in_dim / config.group_size;
    let scales = vb.get_with_hints_dtype(
        (scale_and_zero_size, out_dim),
        if cfg!(feature = "marlin") && marlin_format { "s" } else { "scales" },
        Default::default(),
        DType::F16,
    )?;

    let bias = vb.get_with_hints_dtype((out_dim,), "bias", Default::default(), DType::F16).ok();

    #[cfg(feature = "marlin")]
    let workspace = Tensor::zeros(out_dim / pack_factor!(config.bits), DType::U32, vb.device())?;

    #[cfg(not(feature = "marlin"))]
    let qzeros = vb.get_with_hints_dtype(
        (scale_and_zero_size, out_dim / pack_factor!(config.bits)),
        "qzeros",
        Default::default(),
        DType::I32,
    )?;

    #[cfg(not(feature = "marlin"))]
    let g_idx = vb.get_with_hints_dtype((in_dim,), "g_idx", Default::default(), DType::I32)?;

    #[cfg(feature = "marlin")]
    let config = if marlin_format {
        QuantMethodConfig::Gptq {
            bits: config.bits as i32,
            use_exllama: false,
            q_weight: qweight,
            gptq_qzeros: None,
            gptq_scales: scales,
            g_idx: None,
            bias,
            workspace: Some(workspace),
            is_marlin: true,
        }
    } else {
        let (qweight, scales, qzeros, g_idx) = repack_to_marlin(
            qweight,
            scales,
            vb,
            in_dim,
            out_dim,
            config,
            marlin_compatible,
        )?;

        QuantMethodConfig::Gptq {
            bits: config.bits as i32,
            use_exllama: false,
            q_weight: qweight,
            gptq_qzeros: Some(qzeros),
            gptq_scales: scales,
            g_idx: Some(g_idx),
            bias,
            workspace: if marlin_compatible { Some(workspace) } else { None },
            is_marlin: marlin_compatible,
        }
    };

    #[cfg(not(feature = "marlin"))]
    let config = QuantMethodConfig::Gptq {
        bits: config.bits as i32,
        use_exllama: false,
        q_weight: qweight,
        gptq_qzeros: Some(qzeros),
        gptq_scales: scales,
        g_idx: Some(g_idx),
        bias,
        workspace: None,
        is_marlin: false,
    };

    Ok(Arc::new(GptqLayer::new(config)?))
}

#[cfg(feature = "marlin")]
fn repack_to_marlin(
    qweight: Tensor,
    scales: Tensor,
    vb: VarBuilder,
    in_dim: usize,
    out_dim: usize,
    config: &QuantizedConfig,
    marlin_compatible: bool,
) -> Result<(Tensor, Tensor, Tensor, Tensor)> {
    let qzeros = vb.get_with_hints_dtype(
        (in_dim / config.group_size, out_dim / pack_factor!(config.bits)),
        "qzeros",
        Default::default(),
        DType::I32,
    )?;

    let g_idx = vb.get_with_hints_dtype((in_dim,), "g_idx", Default::default(), DType::I32)?;
    let perm = g_idx
        .to_device(&Device::Cpu)?
        .arg_sort_last_dim(true)?
        .to_device(g_idx.device())?;

    let qweight = if marlin_compatible {
        gptq_weight_repack(&qweight, &perm, in_dim, config.bits as i32)?
    } else {
        qweight
    };

    let scales = if marlin_compatible {
        marlin_permute_scales(
            &scales,
            in_dim / pack_factor!(config.bits),
            out_dim,
            config.group_size as i32,
            config.bits as u32,
        )?
    } else {
        scales
    };

    Ok((qweight, scales, qzeros, g_idx))
}

fn get_scale_perms() -> (Vec<u32>, Vec<u32>) {
    let mut scale_perm: Vec<u32> = Vec::new();
    for i in 0..8 {
        scale_perm.extend((0..8).map(|j| i + 8 * j));
    }
    let mut scale_perm_single: Vec<u32> = Vec::new();
    for i in 0..4 {
        scale_perm_single.extend([0, 1, 8, 9, 16, 17, 24, 25].iter().map(|&j| 2 * i + j));
    }
    (scale_perm, scale_perm_single)
}

fn marlin_permute_scales(
    s: &Tensor,
    size_k: usize,
    size_n: usize,
    group_size: i32,
    _num_bits: u32,
) -> Result<Tensor> {
    let (scale_perm, scale_perm_single) = get_scale_perms();
    let s = if (group_size as usize) < size_k && group_size != -1 {
        let s = s.reshape(((), scale_perm.len()))?;
        let scale_perm_tensor =
            Tensor::from_slice(&scale_perm, scale_perm.len(), s.device())?;
        s.index_select(&scale_perm_tensor, 1)?
    } else {
        let s = s.reshape(((), scale_perm_single.len()))?;
        let scale_perm_single_tensor =
            Tensor::from_slice(&scale_perm_single, scale_perm_single.len(), s.device())?;
        s.index_select(&scale_perm_single_tensor, 1)?
    };

    let s = s.reshape(((), size_n))?.contiguous()?;
    Ok(s)
}
