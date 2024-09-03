#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use candle_core::{quantized::QMatMul, DType, Device, Result, Tensor, IndexOp};
use candle_nn::{embedding, Embedding, Module, VarBuilder};
use mistralrs_quant::{QuantMethod, QuantizedConfig};
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    amoe::{
        AnyMoeBaseModelMixin, AnyMoeConfig, AnyMoeExpertType, AnyMoeTrainableLayer, MlpLayer,
        MoeMlp,
    },
    device_map::DeviceMapper,
    get_delta_from_lora_ab,
    layers::{
        repeat_kv, CausalMasker, Llama3RopeConfig, Llama3RotaryEmbedding, MatMul, RmsNorm,
        ScaledDotProductAttention,
    },
    layers_masker::PastKvLenCache,
    paged_attention::{AttentionImplementation, ModelConfigMetadata, PagedAttention},
    pipeline::{
        extract_logits, text_models_inputs_processor::PagedAttentionInputMetadata, IsqModel,
        NormalLoadingMetadata, NormalModel,
    },
    utils::progress::NiceProgressBar,
};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    pub hidden_size: usize,
    pub intermediate_size: usize,
    pub vocab_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub num_key_value_heads: usize,
    pub use_flash_attn: bool,
    pub rms_norm_eps: f64,
    pub rope_theta: f32,
    pub max_position_embeddings: usize,
    pub rope_scaling: Option<Llama3RopeConfig>,
    pub quantization_config: Option<QuantizedConfig>,
}

struct CausalSelfAttention {
    q_proj: Arc<dyn QuantMethod>,
    k_proj: Arc<dyn QuantMethod>,
    v_proj: Arc<dyn QuantMethod>,
    o_proj: Arc<dyn QuantMethod>,
    num_attention_heads: usize,
    num_key_value_heads: usize,
    head_dim: usize,
    use_flash_attn: bool,
    rotary_emb: Arc<Llama3RotaryEmbedding>,
    max_seq_len: usize,
    paged_attn: Option<PagedAttention>,
}

impl CausalSelfAttention {
    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        x: &Tensor,
        attention_mask: &Option<Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        block_idx: usize,
        kv_cache: &mut crate::pipeline::LayerCaches,
        metadata: Option<((Tensor, Tensor), &mut PagedAttentionInputMetadata)>,
    ) -> Result<Tensor> {
        let (b_sz, seq_len, _) = x.dims3()?;

        let original_dtype = x.dtype();
        let mut x = x.clone();
        if let Some(t) = self.q_proj.quantized_act_type() {
            x = x.to_dtype(t)?;
        }
        let mut q = MatMul.qmethod_matmul(&x, &*self.q_proj)?;
        let mut k = MatMul.qmethod_matmul(&x, &*self.k_proj)?;
        let mut v = MatMul.qmethod_matmul(&x, &*self.v_proj)?;
        if self.q_proj.quantized_act_type().is_some() {
            q = q.to_dtype(original_dtype)?;
            k = k.to_dtype(original_dtype)?;
            v = v.to_dtype(original_dtype)?;
        }

        let mut q = q.reshape((b_sz * seq_len, self.num_attention_heads, self.head_dim))?;
        let mut k = k.reshape((b_sz * seq_len, self.num_key_value_heads, self.head_dim))?;
        let v = if seq_len != 1 {
            v.reshape((b_sz, seq_len, self.num_key_value_heads, self.head_dim))?
                .transpose(1, 2)?
        } else {
            // Optimization for seqlen = 1, avoid transpose and just modify reshape dims
            v.reshape((b_sz, self.num_key_value_heads, seq_len, self.head_dim))?
        };

        self.rotary_emb
            .forward(seqlen_offsets, &start_offsets_kernel, &mut q, &mut k, b_sz)?;

        if q.rank() == 3 && seq_len != 1 {
            q = q
                .reshape((b_sz, seq_len, self.num_attention_heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?;
            k = k
                .reshape((b_sz, seq_len, self.num_key_value_heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?;
        } else if q.rank() == 3 {
            // Optimization for seqlen = 1, avoid transpose and just modify reshape dims
            q = q
                .reshape((b_sz, self.num_attention_heads, seq_len, self.head_dim))?
                .contiguous()?;
            k = k
                .reshape((b_sz, self.num_key_value_heads, seq_len, self.head_dim))?
                .contiguous()?;
        }

        let mut y = match &self.paged_attn {
            Some(paged_attn) => {
                let ((key_cache, value_cache), input_metadata) = metadata.unwrap();
                paged_attn.forward(
                    &q,
                    &k,
                    &v,
                    attention_mask.clone().as_ref(),
                    Some(key_cache),
                    Some(value_cache),
                    input_metadata,
                )?
            }
            None => {
                let (k, v) =
                    crate::pipeline::Cache::update_kv_cache(&mut kv_cache[block_idx], k, v, false)?;

                let k = repeat_kv(k, self.num_attention_heads / self.num_key_value_heads)?
                    .contiguous()?;
                let v = repeat_kv(v, self.num_attention_heads / self.num_key_value_heads)?
                    .contiguous()?;

                ScaledDotProductAttention.run_attention(
                    &q,
                    &k,
                    &v,
                    self.num_attention_heads,
                    self.head_dim,
                    attention_mask.clone().as_ref(),
                    self.use_flash_attn,
                    b_sz,
                    seq_len,
                )?
            }
        };

        if let Some(t) = self.q_proj.quantized_act_type() {
            y = y.to_dtype(t)?;
        }
        y = if attention_mask.is_some() {
            y.transpose(1, 2)?.reshape((b_sz, seq_len, ()))?
        } else {
            y.reshape((b_sz, seq_len, ()))?
        };
        let mut res = MatMul.qmethod_matmul(&y, &*self.o_proj)?;
        if self.q_proj.quantized_act_type().is_some() {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }

    fn load(
        vb: VarBuilder,
        cfg: &Config,
        rope: Arc<Llama3RotaryEmbedding>,
        paged_attn: Option<PagedAttention>,
    ) -> Result<Self> {
        let size_in = cfg.hidden_size;
        let size_q = (cfg.hidden_size / cfg.num_attention_heads) * cfg.num_attention_heads;
        let size_kv = (cfg.hidden_size / cfg.num_attention_heads) * cfg.num_key_value_heads;
        let q_proj = mistralrs_quant::linear_no_bias(
            size_in,
            size_q,
            &cfg.quantization_config,
            vb.pp("q_proj"),
        )?;
        let k_proj = mistralrs_quant::linear_no_bias(
            size_in,
            size_kv,
            &cfg.quantization_config,
            vb.pp("k_proj"),
        )?;
        let v_proj = mistralrs_quant::linear_no_bias(
            size_in,
            size_kv,
            &cfg.quantization_config,
            vb.pp("v_proj"),
        )?;
        let o_proj = mistralrs_quant::linear_no_bias(
            size_q,
            size_in,
            &cfg.quantization_config,
            vb.pp("o_proj"),
        )?;
        Ok(Self {
            q_proj,
            k_proj,
            v_proj,
            o_proj,
            num_attention_heads: cfg.num_attention_heads,
            num_key_value_heads: cfg.num_key_value_heads,
            head_dim: cfg.hidden_size / cfg.num_attention_heads,
            use_flash_attn: cfg.use_flash_attn,
            rotary_emb: rope,
            max_seq_len: cfg.max_position_embeddings,
            paged_attn,
        })
    }
}

#[derive(Clone)]
struct Mlp {
    c_fc1: Arc<dyn QuantMethod>,
    c_fc2: Arc<dyn QuantMethod>,
    c_proj: Arc<dyn QuantMethod>,
    params: Vec<usize>,
}

impl Mlp {
    fn load(vb: VarBuilder, cfg: &Config) -> Result<Self> {
        let h_size = cfg.hidden_size;
        let i_size = cfg.intermediate_size;
        let c_fc1 = mistralrs_quant::linear_no_bias(
            h_size,
            i_size,
            &cfg.quantization_config,
            vb.pp("gate_proj"),
        )?;
        let c_fc2 = mistralrs_quant::linear_no_bias(
            h_size,
            i_size,
            &cfg.quantization_config,
            vb.pp("up_proj"),
        )?;
        let c_proj = mistralrs_quant::linear_no_bias(
            i_size,
            h_size,
            &cfg.quantization_config,
            vb.pp("down_proj"),
        )?;
        Ok(Self {
            c_fc1,
            c_fc2,
            c_proj,
            params: vec![h_size, i_size],
        })
    }
}

impl AnyMoeTrainableLayer for Mlp {}

impl MlpLayer for Mlp {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let original_dtype = x.dtype();
        let mut x = x.clone();
        if let Some(t) = self.c_fc1.quantized_act_type() {
            x = x.to_dtype(t)?;
        }
        let x = (candle_nn::ops::silu(&MatMul.qmethod_matmul(&x, &*self.c_fc1)?)?
            * MatMul.qmethod_matmul(&x, &*self.c_fc2)?)?;
        let mut res = MatMul.qmethod_matmul(&x, &*self.c_proj)?;
        if self.c_fc1.quantized_act_type().is_some() {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }
    fn get_isq_tensors(&mut self) -> Vec<&mut QMatMul> {
        {
            let c_fc1 = self.c_fc1.clone().convert_to_isq().unwrap();
            self.c_fc1 = c_fc1;
            let c_fc2 = self.c_fc2.clone().convert_to_isq().unwrap();
            self.c_fc2 = c_fc2;
            let c_proj = self.c_proj.clone().convert_to_isq().unwrap();
            self.c_proj = c_proj;
        }
        vec![
            Arc::get_mut(&mut self.c_fc1).unwrap().get_qmatmul(),
            Arc::get_mut(&mut self.c_fc2).unwrap().get_qmatmul(),
            Arc::get_mut(&mut self.c_proj).unwrap().get_qmatmul(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
    }
    fn get_isq_biases(&mut self) -> Vec<Option<&mut Tensor>> {
        vec![None, None, None]
    }
    fn clone(&self) -> Box<dyn MlpLayer> {
        Box::new(Clone::clone(self))
    }
    fn get_params(&self) -> &[usize] {
        &self.params
    }
    // c_fc1, c_fc2, c_proj
    fn new_added_delta(&self, deltas: Vec<Option<Tensor>>) -> Result<Box<dyn MlpLayer>> {
        let new_c_fc1 = if let Some(ref delta) = deltas[0] {
            self.c_fc1.add_delta_w(delta)?
        } else {
            self.c_fc1.clone()
        };
        let new_c_fc2 = if let Some(ref delta) = deltas[1] {
            self.c_fc2.add_delta_w(delta)?
        } else {
            self.c_fc2.clone()
        };
        let new_c_proj = if let Some(ref delta) = deltas[2] {
            self.c_proj.add_delta_w(delta)?
        } else {
            self.c_proj.clone()
        };

        Ok(Box::new(Self {
            c_fc1: new_c_fc1,
            c_fc2: new_c_fc2,
            c_proj: new_c_proj,
            params: self.params.clone(),
        }))
    }

    fn dtype_device(&self) -> (DType, Device) {
        self.c_fc1.dtype_and_device()
    }
}

struct Block {
    rms_1: RmsNorm,
    attn: CausalSelfAttention,
    rms_2: RmsNorm,
    mlp: Box<dyn MlpLayer>,
}

impl Block {
    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        x: &Tensor,
        attention_mask: &Option<Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        block_idx: usize,
        kv_cache: &mut crate::pipeline::LayerCaches,
        metadata: Option<((Tensor, Tensor), &mut PagedAttentionInputMetadata)>,
    ) -> Result<Tensor> {
        let residual = x;
        let x = self.rms_1.forward(x)?;
        let x = (self.attn.forward(
            &x,
            attention_mask,
            seqlen_offsets,
            start_offsets_kernel,
            block_idx,
            kv_cache,
            metadata,
        )? + residual)?;
        let residual = &x;
        // let x = (self.mlp.forward(&self.rms_2.forward(&x)?)? + residual)?;
        let x = self.rms_2.forward(&x)?;
        Ok(x)
    }

    fn load(
        vb: VarBuilder,
        cfg: &Config,
        mapper: &dyn DeviceMapper,
        layer_idx: usize,
        loading_isq: bool,
        rope: Arc<Llama3RotaryEmbedding>,
        paged_attn: Option<PagedAttention>,
    ) -> Result<Self> {
        let attn = CausalSelfAttention::load(
            mapper.set_device(layer_idx, vb.pp("self_attn"), loading_isq),
            cfg,
            rope,
            paged_attn,
        )?;
        let mlp = Mlp::load(mapper.set_device(layer_idx, vb.pp("mlp"), loading_isq), cfg)?;
        let rms_1 = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_device(layer_idx, vb.pp("input_layernorm"), false),
        )?;
        let rms_2 = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_device(layer_idx, vb.pp("post_attention_layernorm"), false),
        )?;
        Ok(Self {
            rms_1,
            attn,
            rms_2,
            mlp: Box::new(mlp),
        })
    }
}

pub struct Llama {
    wte: Embedding,
    blocks: Vec<Block>,
    ln_f: RmsNorm,
    lm_head: QMatMul,
    // pub kv_cache: crate::pipeline::Cache,
    pub kv_caches: Vec<crate::pipeline::Cache>,
    cuda_devices: Vec<candle_core::Device>,
    pub device: Device,
    mapper: Box<dyn DeviceMapper + Send + Sync>,
    cfg: ModelConfigMetadata,
}

impl Llama {
    pub fn forward(
        &self,
        input_ids: &Tensor,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
        mut metadata: Option<(Vec<(Tensor, Tensor)>, &mut PagedAttentionInputMetadata)>,
    ) -> Result<Tensor> {
        let mut x = self.wte.forward(input_ids)?;
        let (batch_size, seq_len, hidden_size) = x.dims3()?;

        let num_devices = 4;
        let chunk_size = seq_len / num_devices;

        let mut chunks: Vec<Tensor> = Vec::with_capacity(num_devices);
        // chunks.push(x.copy().unwrap());

        // Handle the case where sequence length is less than number of devices
        if seq_len <= num_devices {
            for j in 0..seq_len {
                // let chunk = x.i((.., j..j+1, ..))?;
                let chunk = x.clone();
                chunks.push(chunk.to_device(&self.cuda_devices[j])?);
            }
        } else {
            for j in 0..num_devices {
                let start = j * chunk_size;
                let end = if j == num_devices - 1 {
                    seq_len
                } else {
                    (j+ 1) * chunk_size
                };
    
                let chunk = x.i((.., start..end,..))?;
                let device = &self.cuda_devices[j];
                chunks.push(chunk.to_device(&device)?);
            }
        }

        // let mut cache = self.kv_caches[0].lock();
        let mut processed_chunks = Vec::new();
        let mut target_device = &self.cuda_devices[0];

        let mut block_chunks: Vec<Tensor> = Vec::new();

        for (block_idx, block) in self.blocks.iter().enumerate() {
            
            // x = self.mapper.map(x, block_idx)?;
            // x = self.mapper.map(&chunks[0], block_idx)?;
            println!("block_idx {:?}", block_idx);
            // println!("chunk device {:?}", chunks[0].device());
            for (chunk_idx, chunk) in chunks.iter().enumerate() {
                println!("chunk_idx {:?}", chunk_idx);
                let mut x = if block_idx == 0 {
                    let tensor = chunk.clone();
                    self.mapper.map(tensor.clone(), block_idx)?;
                    tensor
                } else {
                    let tensor = block_chunks[chunk_idx].clone();
                    self.mapper.map(tensor.clone(), block_idx)?;
                    tensor
                };

                let num_caches = self.kv_caches.len();

                for cache_rotation in 0..num_caches {
                    let cache_idx = (chunk_idx + cache_rotation) % num_caches;
                    let kv_cache = &self.kv_caches[cache_idx];
                    println!("cache_idx {:?}", cache_idx);
                    let mut cache = kv_cache.lock();

                    let device_chunk = chunk.device();

                    // Determine the original device of the cache
                    let original_cache_device = cache.iter().find_map(|opt| {
                        opt.as_ref().map(|(k, _)| k.device().clone())
                    }).unwrap_or_else(|| device_chunk.clone());

                    // Move cache to chunk device
                    let mut cache_on_chunk_device: Vec<_> = cache.iter().map(|opt| {
                        opt.as_ref().map(|(k, v)| {
                            (k.to_device(device_chunk).unwrap(), v.to_device(device_chunk).unwrap())
                        })
                    }).collect();

                    let mask = CausalMasker.make_causal_mask_as_attn_bias(
                        input_ids,
                        metadata
                            .as_ref()
                            .map(|(_, _)| &seqlen_offsets as &dyn PastKvLenCache)
                            .unwrap_or(&*cache as &dyn PastKvLenCache),
                        // x.dtype(),
                        chunks[0].dtype(),
                        self.blocks[0].attn.num_attention_heads,
                    )?;


            
                
                    // x = block.forward(
                    //     &x,
                    //     &mask.clone().map(|m| m.to_device(x.device()).unwrap()),
                    //     seqlen_offsets,
                    //     start_offsets_kernel.clone(),
                    //     block_idx,
                    //     &mut cache,
                    //     metadata
                    //         .as_mut()
                    //         .map(|(kv_cache, metadata)| (kv_cache[block_idx].clone(), &mut **metadata)),
                    // )?;

                    println!("before block forward");
                    x = block.forward(
                        &x,
                        &mask.clone().map(|m| m.to_device(&device_chunk).unwrap()),
                        seqlen_offsets,
                        start_offsets_kernel.clone().to_device(&device_chunk)?,
                        block_idx,
                        // &mut cache_on_chunk_device,
                        &mut cache_on_chunk_device,
                        metadata
                            .as_mut()
                            .map(|(kv_cache, metadata)| {
                                let (tensor1, tensor2) = kv_cache[block_idx].clone();
                                (
                                    (tensor1.to_device(&device_chunk).unwrap(), tensor2.to_device(&device_chunk).unwrap()),
                                    &mut **metadata
                                )
                            }),
                    )?;
                
                    println!("after block forward");

                    // Accumulate attention results
                    if block_chunks.len() <= chunk_idx {
                        block_chunks.push(x.clone());
                    } else {
                        block_chunks[chunk_idx] = x.clone();
                    }
                }
            }   

            // Concatenate chunks for this block
            // let block_chunks: Result<Vec<Tensor>> = block_chunks
            //     .clone()
            //     .into_iter()
            //     .map(|chunk| chunk.to_device(&device_chunk))
            //     .collect();
        
            // let block_chunks = block_chunks?; // Propagate any errors

            let mut x = candle_core::Tensor::cat(&block_chunks, 1)?;

            // do feedforward after attention has been run for each chunk
            let residual = x.clone();
            let mut x = block.mlp.forward(&x)?;
            x = (x + &residual)?;
            x = x.to_device(&target_device)?;
            processed_chunks.push(x.clone()); 
        }
        x = candle_core::Tensor::cat(&processed_chunks, 1)?;
        let x = x.to_device(&self.device)?;
        let mut x = self.ln_f.forward(&x)?;
        if matches!(self.lm_head, QMatMul::QTensor(_)) {
            x = x.to_dtype(DType::F32)?;
        }
        let logits = MatMul.qmatmul(&x, &self.lm_head)?;
        extract_logits(&logits, context_lens)
    }

    pub fn new(
        cfg: &Config,
        vb: VarBuilder,
        is_gptx: bool,
        normal_loading_metadata: NormalLoadingMetadata,
        attention_mechanism: AttentionImplementation,
    ) -> Result<Self> {
        if let Some(ref quant_cfg) = &cfg.quantization_config {
            tracing::info!(
                "Using {} quantization in {} bits.",
                quant_cfg.quant_method.to_string(),
                quant_cfg.bits
            );
        }

        let num_devices = 4;
        let mut cuda_devices = Vec::with_capacity(num_devices);
        let mapper = normal_loading_metadata
            .mapper
            .into_mapper(cfg.num_hidden_layers, &normal_loading_metadata.real_device)?;

        let wte = embedding(
            cfg.vocab_size,
            cfg.hidden_size,
            mapper.set_nm_device(vb.pp("model.embed_tokens"), false),
        )?;
        let lm_head = candle_nn::linear_no_bias(
            cfg.hidden_size,
            cfg.vocab_size,
            mapper.set_nm_device(vb.pp("lm_head"), normal_loading_metadata.loading_isq),
        )?;
        let ln_f = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_nm_device(vb.pp("model.norm"), false),
        )?;
        let head_dim = cfg.hidden_size / cfg.num_attention_heads;
        let blocks: Vec<_> =
            NiceProgressBar::<_, 'b'>(0..cfg.num_hidden_layers, "Loading repeating layers")
                .into_iter()
                .map(|i| {
                    let device = mapper
                        .device_for(i, false)
                        .unwrap_or(&normal_loading_metadata.real_device);
                    let rotary_emb = Arc::new(
                        Llama3RotaryEmbedding::new(vb.dtype(), cfg, device, is_gptx)
                            .expect("Failed to create RoPE"),
                    );
                    let paged_attn = match &attention_mechanism {
                        AttentionImplementation::Eager => None,
                        AttentionImplementation::PagedAttention => Some(
                            PagedAttention::new(
                                cfg.num_attention_heads,
                                head_dim,
                                (1.0 / (head_dim as f64).sqrt()) as f32,
                                Some(cfg.num_key_value_heads),
                                None,
                                device,
                                None,
                            )
                            .expect("Failed to create PagedAttention"),
                        ),
                    };
                    if !cuda_devices.iter().any(|d| format!("{:?}", d) == format!("{:?}", device)) {
                        cuda_devices.push(device.clone());
                    }
                    Block::load(
                        vb.pp(&format!("model.layers.{i}")),
                        cfg,
                        &*mapper,
                        i,
                        normal_loading_metadata.loading_isq,
                        rotary_emb,
                        paged_attn,
                    )
                    .expect("Failed to load block.")
                })
                .collect();

                let mut kv_caches: Vec<crate::pipeline::Cache> = Vec::with_capacity(num_devices);

                for device_id in 0..num_devices {
                    let cache = crate::pipeline::Cache::new(cfg.num_hidden_layers , false);
                    kv_caches.push(cache);
                };

        Ok(Self {
            wte,
            blocks,
            ln_f,
            lm_head: QMatMul::Tensor(lm_head.weight().clone()),
            // kv_cache: crate::pipeline::Cache::new(cfg.num_hidden_layers, false),
            kv_caches,
            cuda_devices,
            device: normal_loading_metadata.real_device,
            mapper,
            cfg: ModelConfigMetadata {
                num_layers: cfg.num_hidden_layers,
                hidden_size: cfg.hidden_size,
                num_kv_heads: cfg.num_key_value_heads,
                num_attn_heads: cfg.num_attention_heads,
                sliding_window: None,
            },
        })
    }
}

impl IsqModel for Llama {
    fn get_matmuls(&mut self) -> (Vec<(&mut QMatMul, Option<usize>)>, &dyn DeviceMapper) {
        let mut tensors = Vec::new();
        tensors.push((&mut self.lm_head, None));
        for (i, layer) in self.blocks.iter_mut().enumerate() {
            {
                let q_proj = layer.attn.q_proj.clone().convert_to_isq().unwrap();
                layer.attn.q_proj = q_proj;
                let k_proj = layer.attn.k_proj.clone().convert_to_isq().unwrap();
                layer.attn.k_proj = k_proj;
                let v_proj = layer.attn.v_proj.clone().convert_to_isq().unwrap();
                layer.attn.v_proj = v_proj;
                let o_proj = layer.attn.o_proj.clone().convert_to_isq().unwrap();
                layer.attn.o_proj = o_proj;
            }
            if let Some(q) = Arc::get_mut(&mut layer.attn.q_proj).unwrap().get_qmatmul() {
                tensors.push((q, Some(i)));
            }
            if let Some(k) = Arc::get_mut(&mut layer.attn.k_proj).unwrap().get_qmatmul() {
                tensors.push((k, Some(i)));
            }
            if let Some(b) = Arc::get_mut(&mut layer.attn.v_proj).unwrap().get_qmatmul() {
                tensors.push((b, Some(i)));
            }
            if let Some(o) = Arc::get_mut(&mut layer.attn.o_proj).unwrap().get_qmatmul() {
                tensors.push((o, Some(i)));
            }
            tensors.extend(
                layer
                    .mlp
                    .get_isq_tensors()
                    .into_iter()
                    .map(|m| (m, Some(i)))
                    .collect::<Vec<_>>(),
            );
        }
        (tensors, &*self.mapper)
    }
    fn get_biases(&mut self) -> (Vec<(Option<&mut Tensor>, Option<usize>)>, &dyn DeviceMapper) {
        (Vec::new(), &*self.mapper)
    }
}

impl NormalModel for Llama {
    fn forward(
        &self,
        input_ids: &Tensor,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
        _position_ids: Vec<usize>,
        metadata: Option<(Vec<(Tensor, Tensor)>, &mut PagedAttentionInputMetadata)>,
    ) -> Result<Tensor> {
        self.forward(
            input_ids,
            seqlen_offsets,
            start_offsets_kernel,
            context_lens,
            metadata,
        )
    }
    fn xlora_forward(
        &self,
        _input_ids: &Tensor,
        _input_ids_full: &Tensor,
        _seqlen_offsets: &[usize],
        _seqlen_offsets_full: &[usize],
        _start_offsets_kernel: Tensor,
        _start_offsets_kernel_full: Tensor,
        _no_kv_cache: bool,
        _non_granular_state: &Option<crate::xlora_models::NonGranularState>,
        _context_lens: Vec<(usize, usize)>,
        _position_ids: Vec<usize>,
    ) -> Result<Tensor> {
        unimplemented!()
    }
    fn cache(&self) -> &crate::pipeline::Cache {
        &self.kv_caches[0]
        // &self.kv_cache
    }
    fn device(&self) -> &Device {
        &self.device
    }
    fn is_xlora(&self) -> bool {
        false
    }
    fn max_seq_len(&self) -> usize {
        self.blocks[0].attn.max_seq_len
    }
    fn config(&self) -> &ModelConfigMetadata {
        &self.cfg
    }
}

impl AnyMoeBaseModelMixin for Llama {
    fn get_mlps(&self) -> Vec<&dyn MlpLayer> {
        let mut mlps = Vec::new();
        for layer in &self.blocks {
            mlps.push(&*layer.mlp);
        }
        mlps
    }
    fn get_mlps_mut(&mut self) -> Vec<&mut Box<dyn MlpLayer>> {
        let mut mlps = Vec::new();
        for layer in &mut self.blocks {
            mlps.push(&mut layer.mlp);
        }
        mlps
    }
    fn create_anymoe_layers(
        &mut self,
        additional_vbs: Vec<VarBuilder>,
        config: AnyMoeConfig,
        (prefix, mlp): (String, String),
        mut layers: Vec<usize>,
        expert_type: AnyMoeExpertType,
        gate_vb: Option<VarBuilder>,
    ) -> Result<()> {
        let mut experts: Vec<Vec<Box<dyn MlpLayer>>> = Vec::new();
        if layers.is_empty() {
            layers = (0..self.blocks.len()).collect::<Vec<_>>();
        }
        for _ in 0..layers.len() {
            experts.push(Vec::new());
        }
        for vb in additional_vbs {
            let vb = vb.pp(&prefix);
            for (layer, row) in experts.iter_mut().enumerate() {
                if !layers.contains(&layer) {
                    continue;
                }

                let intermediate_size = self.blocks[layer].mlp.get_params()[1];
                let hidden_size = self.blocks[layer].mlp.get_params()[0];
                match expert_type {
                    AnyMoeExpertType::FineTuned => {
                        let (dtype, device) = self.blocks[layer].mlp.dtype_device();
                        row.push(Box::new(Mlp::load(
                            vb.pp(layer).pp(&mlp).set_dtype(dtype).set_device(device),
                            &Config {
                                intermediate_size: self.blocks[layer].mlp.get_params()[1],
                                hidden_size: self.blocks[layer].mlp.get_params()[0],
                                ..Default::default()
                            },
                        )?));
                    }
                    AnyMoeExpertType::LoraAdapter {
                        rank,
                        alpha,
                        ref target_modules,
                    } => {
                        let vb_mlp = vb.pp(layer).pp(&mlp);

                        let c_fc1_delta = if target_modules.contains(&"c_fc1".to_string()) {
                            Some(get_delta_from_lora_ab!(
                                vb_mlp,
                                rank,
                                alpha,
                                (hidden_size, intermediate_size),
                                "c_fc1"
                            ))
                        } else {
                            None
                        };
                        let c_fc2_delta = if target_modules.contains(&"c_fc2".to_string()) {
                            Some(get_delta_from_lora_ab!(
                                vb_mlp,
                                rank,
                                alpha,
                                (hidden_size, intermediate_size),
                                "c_fc2"
                            ))
                        } else {
                            None
                        };
                        let c_proj_delta = if target_modules.contains(&"c_proj".to_string()) {
                            Some(get_delta_from_lora_ab!(
                                vb_mlp,
                                rank,
                                alpha,
                                (intermediate_size, hidden_size),
                                "c_proj"
                            ))
                        } else {
                            None
                        };

                        row.push(self.blocks[layer].mlp.new_added_delta(vec![
                            c_fc1_delta,
                            c_fc2_delta,
                            c_proj_delta,
                        ])?);
                    }
                }
            }
        }
        for (layer, expert) in layers.into_iter().zip(experts) {
            let mut experts_all = vec![self.blocks[layer].mlp.clone()];
            experts_all.extend(expert);
            let (dtype, device) = self.blocks[layer].mlp.dtype_device();
            self.blocks[layer].mlp = Box::new(MoeMlp::new(
                experts_all,
                config.clone(),
                dtype,
                &device,
                layer,
                gate_vb.as_ref(),
            )?);
        }
        Ok(())
    }
    fn amoe_supported(&self) -> bool {
        true
    }
}
