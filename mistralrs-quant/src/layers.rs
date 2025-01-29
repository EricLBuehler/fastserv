use std::sync::Arc;

use candle_core::{Context, Result, Tensor};
use candle_nn::{var_builder::ShardedVarBuilder, Linear};

use crate::{
    blockwise_fp8::blockwise_fp8_linear_b, distributed, gptq::gptq_linear, BnbLinear, DummyLayer,
    QuantMethod, QuantMethodConfig, QuantMethodType, QuantizedConfig, QuantizedSerde,
    UnquantLinear,
};

fn shard(dim: usize, rank: usize, world_size: usize) -> candle_nn::var_builder::Shard {
    candle_nn::var_builder::Shard {
        dim,
        rank,
        world_size,
    }
}

/// This layer has a weight that is parallelized along the input dimension,
/// returning the "full" output dimension.
#[derive(Debug)]
pub struct RowParallelLayer {
    weight: Arc<dyn QuantMethod>,
    bias: Option<Tensor>,
    all_reduce: distributed::AllReduce,
}

impl RowParallelLayer {
    pub fn new(
        in_dim: usize,
        out_dim: usize,
        config: &Option<QuantizedConfig>,
        bias: bool,
        comm: &Arc<crate::Comm>,
        vb: ShardedVarBuilder,
    ) -> Result<Arc<dyn QuantMethod>> {
        let rank = comm.rank();
        let world_size = comm.world_size();
        let shard = shard(1, rank, world_size);

        let weight = if let Some(quant_conf) = &config {
            // GPTQ and BNB do not support tensor parallelism
            if matches!(
                quant_conf.quant_method,
                QuantMethodType::Bitsandbytes | QuantMethodType::Gptq
            ) && comm.world_size() != 1
            {
                candle_core::bail!(
                    "GPTQ and BNB quantization types to not support tensor parallelism, but got a world size of {}",
                    comm.world_size()
                );
            }

            match quant_conf.quant_method {
                QuantMethodType::Gptq => {
                    let gpt_layer = gptq_linear(in_dim, out_dim, quant_conf, vb.clone())?;
                    return Ok(gpt_layer);
                }
                QuantMethodType::Bitsandbytes => {
                    let bnb_layer =
                        Arc::new(BnbLinear::linear_b(in_dim, out_dim, bias, vb.clone())?) as Arc<_>;
                    return Ok(bnb_layer);
                }
                QuantMethodType::Fp8 => {
                    // NOTE: no bias for fp8 as it might be parallelized
                    blockwise_fp8_linear_b(in_dim, out_dim, quant_conf, false, shard, vb.clone())?
                }
                QuantMethodType::Unreachable => unreachable!(),
            }
        } else {
            // Handle the case where the layer is dummy (no tensors)
            if !vb.contains_tensor("weight") {
                let layer = <DummyLayer as QuantMethod>::new(QuantMethodConfig::Dummy)?;
                Arc::new(layer) as Arc<dyn QuantMethod>
            } else {
                let weight = vb.get_with_hints((out_dim, in_dim), "weight", shard)?;

                let layer = <UnquantLinear as QuantMethod>::new(QuantMethodConfig::Unquantized(
                    Linear::new(weight, None),
                ))?;
                Arc::new(layer) as Arc<dyn QuantMethod>
            }
        };

        let bias = if bias {
            Some(vb.get((out_dim,), "bias")?)
        } else {
            None
        };

        Ok(Arc::new(Self {
            weight,
            bias,
            all_reduce: distributed::AllReduce::new(comm),
        }))
    }
}

impl QuantMethod for RowParallelLayer {
    fn new(_method: QuantMethodConfig) -> Result<Self>
    where
        Self: Sized,
    {
        candle_core::bail!("RowParallelLayer should not be constructed with `QuantMethod::new`")
    }

    fn forward(&self, a: &Tensor) -> Result<Tensor> {
        let mut xs = self
            .weight
            .forward(&a)?
            .apply_op1_no_bwd(&self.all_reduce)?;
        if let Some(bias) = &self.bias {
            xs = xs.broadcast_add(bias)?;
        }
        Ok(xs)
    }

    fn add_delta_w(&self, delta: &Tensor) -> Result<Arc<dyn QuantMethod>> {
        let weight = self.weight.add_delta_w(delta)?;
        Ok(Arc::new(Self {
            weight,
            bias: self.bias.clone(),
            all_reduce: self.all_reduce.clone(),
        }))
    }

    fn dequantize_w(&self) -> Result<Tensor> {
        self.weight.dequantize_w()
    }

    fn dtype_and_device(&self) -> (candle_core::DType, candle_core::Device) {
        self.weight.dtype_and_device()
    }

    fn begin_track_stats(&mut self) -> Result<()> {
        Arc::get_mut(&mut self.weight)
            .context("Failed to get &mut to weight")?
            .begin_track_stats()
    }

    fn end_track_stats(&self) -> Result<Tensor> {
        self.weight.end_track_stats()
    }

    fn quantized_act_type(&self) -> Option<candle_core::DType> {
        self.weight.quantized_act_type()
    }

    fn unquant_weight_bias(&self) -> Option<(Tensor, Option<Tensor>)> {
        self.weight.unquant_weight_bias()
    }

    fn get_max_isq_cpu_threads(&self, dtype: crate::IsqType) -> Option<std::num::NonZeroUsize> {
        self.weight.get_max_isq_cpu_threads(dtype)
    }

    fn apply_isq(
        self: Arc<Self>,
        dtype: Option<crate::IsqType>,
        device: candle_core::Device,
        n_quantized: &std::sync::atomic::AtomicUsize,
        imatrix_weight: Option<Vec<f32>>,
    ) -> Result<Arc<dyn QuantMethod>> {
        let weight = self
            .weight
            .clone()
            .apply_isq(dtype, device, n_quantized, imatrix_weight)?;
        Ok(Arc::new(Self {
            weight,
            bias: self.bias.clone(),
            all_reduce: self.all_reduce.clone(),
        }))
    }
}

impl QuantizedSerde for RowParallelLayer {
    fn isq_serde_supported(&self) -> bool {
        self.weight.isq_serde_supported()
    }
    fn name(&self) -> &'static str {
        self.weight.name()
    }
    fn serialize(&self) -> Result<std::borrow::Cow<[u8]>> {
        self.weight.serialize_with_bias(self.bias.clone())
    }
}

#[derive(Debug)]
/// This layer has a weight that is parallelized along the output dimension,
/// taking the "full" input dimension.
pub struct ColumnParallelLayer {
    weight: Arc<dyn QuantMethod>,
    bias: Option<Tensor>,
}

impl ColumnParallelLayer {
    pub fn new(
        in_dim: usize,
        out_dim: usize,
        config: &Option<QuantizedConfig>,
        bias: bool,
        comm: &Arc<crate::Comm>,
        vb: ShardedVarBuilder,
    ) -> Result<Arc<dyn QuantMethod>> {
        let rank = comm.rank();
        let world_size = comm.world_size();
        let shard = shard(0, rank, world_size);

        let weight = if let Some(quant_conf) = &config {
            // GPTQ and BNB do not support tensor parallelism
            if matches!(
                quant_conf.quant_method,
                QuantMethodType::Bitsandbytes | QuantMethodType::Gptq
            ) && comm.world_size() != 1
            {
                candle_core::bail!(
                "GPTQ and BNB quantization types to not support tensor parallelism, but got a world size of {}",
                comm.world_size()
            );
            }

            match quant_conf.quant_method {
                QuantMethodType::Gptq => {
                    let gpt_layer = gptq_linear(in_dim, out_dim, quant_conf, vb.clone())?;
                    return Ok(gpt_layer);
                }
                QuantMethodType::Bitsandbytes => {
                    let bnb_layer =
                        Arc::new(BnbLinear::linear_b(in_dim, out_dim, bias, vb.clone())?) as Arc<_>;
                    return Ok(bnb_layer);
                }
                QuantMethodType::Fp8 => {
                    // NOTE: no bias for fp8 as it might be parallelized
                    blockwise_fp8_linear_b(
                        in_dim,
                        out_dim,
                        quant_conf,
                        false,
                        shard.clone(),
                        vb.clone(),
                    )?
                }
                QuantMethodType::Unreachable => unreachable!(),
            }
        } else {
            // Handle the case where the layer is dummy (no tensors)
            if !vb.contains_tensor("weight") {
                let layer = <DummyLayer as QuantMethod>::new(QuantMethodConfig::Dummy)?;
                Arc::new(layer) as Arc<dyn QuantMethod>
            } else {
                let weight = vb.get_with_hints((out_dim, in_dim), "weight", shard.clone())?;

                let layer = <UnquantLinear as QuantMethod>::new(QuantMethodConfig::Unquantized(
                    Linear::new(weight, None),
                ))?;
                Arc::new(layer) as Arc<dyn QuantMethod>
            }
        };

        let bias = if bias {
            Some(vb.get_with_hints((out_dim,), "bias", shard)?)
        } else {
            None
        };

        Ok(Arc::new(Self { weight, bias }))
    }
}

impl QuantMethod for ColumnParallelLayer {
    fn new(_method: QuantMethodConfig) -> Result<Self>
    where
        Self: Sized,
    {
        candle_core::bail!("ColumnParallelLayer should not be constructed with `QuantMethod::new`")
    }

    fn forward(&self, a: &Tensor) -> Result<Tensor> {
        let mut xs = self.weight.forward(&a)?;
        if let Some(bias) = &self.bias {
            xs = xs.broadcast_add(bias)?;
        }
        Ok(xs)
    }

    fn add_delta_w(&self, delta: &Tensor) -> Result<Arc<dyn QuantMethod>> {
        let weight = self.weight.add_delta_w(delta)?;
        Ok(Arc::new(Self {
            weight,
            bias: self.bias.clone(),
        }))
    }

    fn dequantize_w(&self) -> Result<Tensor> {
        self.weight.dequantize_w()
    }

    fn dtype_and_device(&self) -> (candle_core::DType, candle_core::Device) {
        self.weight.dtype_and_device()
    }

    fn begin_track_stats(&mut self) -> Result<()> {
        Arc::get_mut(&mut self.weight)
            .context("Failed to get &mut to weight")?
            .begin_track_stats()
    }

    fn end_track_stats(&self) -> Result<Tensor> {
        self.weight.end_track_stats()
    }

    fn quantized_act_type(&self) -> Option<candle_core::DType> {
        self.weight.quantized_act_type()
    }

    fn unquant_weight_bias(&self) -> Option<(Tensor, Option<Tensor>)> {
        self.weight.unquant_weight_bias()
    }

    fn get_max_isq_cpu_threads(&self, dtype: crate::IsqType) -> Option<std::num::NonZeroUsize> {
        self.weight.get_max_isq_cpu_threads(dtype)
    }

    fn apply_isq(
        self: Arc<Self>,
        dtype: Option<crate::IsqType>,
        device: candle_core::Device,
        n_quantized: &std::sync::atomic::AtomicUsize,
        imatrix_weight: Option<Vec<f32>>,
    ) -> Result<Arc<dyn QuantMethod>> {
        let weight = self
            .weight
            .clone()
            .apply_isq(dtype, device, n_quantized, imatrix_weight)?;
        Ok(Arc::new(Self {
            weight,
            bias: self.bias.clone(),
        }))
    }
}

impl QuantizedSerde for ColumnParallelLayer {
    fn isq_serde_supported(&self) -> bool {
        self.weight.isq_serde_supported()
    }
    fn name(&self) -> &'static str {
        self.weight.name()
    }
    fn serialize(&self) -> Result<std::borrow::Cow<[u8]>> {
        self.weight.serialize_with_bias(self.bias.clone())
    }
}

#[derive(Debug)]
/// This layer has no parallelization
pub struct ReplicatedLayer(Arc<dyn QuantMethod>);

impl ReplicatedLayer {
    pub fn new(
        in_dim: usize,
        out_dim: usize,
        config: &Option<QuantizedConfig>,
        bias: bool,
        _comm: &Arc<crate::Comm>,
        vb: ShardedVarBuilder,
    ) -> Result<Arc<dyn QuantMethod>> {
        let layer = if let Some(quant_conf) = &config {
            match quant_conf.quant_method {
                QuantMethodType::Gptq => gptq_linear(in_dim, out_dim, quant_conf, vb.clone())?,
                QuantMethodType::Bitsandbytes => {
                    Arc::new(BnbLinear::linear_b(in_dim, out_dim, bias, vb.clone())?) as Arc<_>
                }
                QuantMethodType::Fp8 => blockwise_fp8_linear_b(
                    in_dim,
                    out_dim,
                    quant_conf,
                    bias,
                    Default::default(),
                    vb.clone(),
                )?,
                QuantMethodType::Unreachable => unreachable!(),
            }
        } else {
            // Handle the case where the layer is dummy (no tensors)
            if !vb.contains_tensor("weight") {
                let layer = <DummyLayer as QuantMethod>::new(QuantMethodConfig::Dummy)?;
                Arc::new(layer) as Arc<dyn QuantMethod>
            } else {
                let weight = vb.get_with_hints((out_dim, in_dim), "weight", Default::default())?;

                let bias = if bias {
                    Some(vb.get_with_hints((out_dim,), "bias", Default::default())?)
                } else {
                    None
                };
                let layer = <UnquantLinear as QuantMethod>::new(QuantMethodConfig::Unquantized(
                    Linear::new(weight, bias),
                ))?;
                Arc::new(layer) as Arc<dyn QuantMethod>
            }
        };

        Ok(Arc::new(Self(layer)))
    }
}

impl QuantMethod for ReplicatedLayer {
    fn new(_method: QuantMethodConfig) -> Result<Self>
    where
        Self: Sized,
    {
        candle_core::bail!("ReplicatedLayer should not be constructed with `QuantMethod::new`")
    }

    fn forward(&self, a: &Tensor) -> Result<Tensor> {
        self.0.forward(a)
    }

    fn add_delta_w(&self, delta: &Tensor) -> Result<Arc<dyn QuantMethod>> {
        self.0.add_delta_w(delta)
    }

    fn dequantize_w(&self) -> Result<Tensor> {
        self.0.dequantize_w()
    }

    fn dtype_and_device(&self) -> (candle_core::DType, candle_core::Device) {
        self.0.dtype_and_device()
    }

    fn begin_track_stats(&mut self) -> Result<()> {
        Arc::get_mut(&mut self.0)
            .context("Failed to get &mut to weight")?
            .begin_track_stats()
    }

    fn end_track_stats(&self) -> Result<Tensor> {
        self.0.end_track_stats()
    }

    fn quantized_act_type(&self) -> Option<candle_core::DType> {
        self.0.quantized_act_type()
    }

    fn unquant_weight_bias(&self) -> Option<(Tensor, Option<Tensor>)> {
        self.0.unquant_weight_bias()
    }

    fn get_max_isq_cpu_threads(&self, dtype: crate::IsqType) -> Option<std::num::NonZeroUsize> {
        self.0.get_max_isq_cpu_threads(dtype)
    }

    fn apply_isq(
        self: Arc<Self>,
        dtype: Option<crate::IsqType>,
        device: candle_core::Device,
        n_quantized: &std::sync::atomic::AtomicUsize,
        imatrix_weight: Option<Vec<f32>>,
    ) -> Result<Arc<dyn QuantMethod>> {
        self.0
            .clone()
            .apply_isq(dtype, device, n_quantized, imatrix_weight)
    }
}

impl QuantizedSerde for ReplicatedLayer {
    fn isq_serde_supported(&self) -> bool {
        self.0.isq_serde_supported()
    }
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn serialize(&self) -> Result<std::borrow::Cow<[u8]>> {
        self.0.serialize()
    }
}
