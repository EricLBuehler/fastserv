use candle_core::{DType, Device, IndexOp, Result, Tensor, D};
use candle_nn::{layer_norm, LayerNorm, Linear, Module, VarBuilder};

use crate::{
    layers::{Activation, Conv3dConfig, Conv3dNoBias, FusedBiasLinear},
    ops::RepeatInterleaveOp,
};

use super::config::VisionConfig;

struct PatchEmbed {
    proj: Conv3dNoBias,
    in_channels: usize,
    patch_size: usize,
    temporal_patch_size: usize,
    embed_dim: usize,
}

// https://github.com/huggingface/transformers/blob/f2c388e3f946862f657acc1e21b272ec946fc66c/src/transformers/models/qwen2_vl/modeling_qwen2_vl.py#L272
impl PatchEmbed {
    fn new(cfg: &VisionConfig, vb: VarBuilder) -> Result<Self> {
        if cfg.temporal_patch_size != 2 {
            candle_core::bail!("Only support temporal patch size of 2");
        }
        Ok(Self {
            proj: Conv3dNoBias::new(
                cfg.in_channels,
                cfg.embed_dim,
                [cfg.temporal_patch_size, cfg.patch_size, cfg.patch_size],
                Conv3dConfig {
                    stride: cfg.patch_size,
                    ..Default::default()
                },
                vb.pp("proj"),
            )?,
            in_channels: cfg.in_channels,
            patch_size: cfg.patch_size,
            temporal_patch_size: cfg.temporal_patch_size,
            embed_dim: cfg.embed_dim,
        })
    }

    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let xs = xs.reshape((
            (),
            self.in_channels,
            self.temporal_patch_size,
            self.patch_size,
            self.patch_size,
        ))?;
        xs.apply(&self.proj)?.reshape(((), self.embed_dim))
    }
}

// https://github.com/huggingface/transformers/blob/a769ed45e17c44fd17b85c025863c4e4f2f73634/src/transformers/models/qwen2_vl/modeling_qwen2_vl.py#L314
struct VisionMlp {
    fc1: FusedBiasLinear,
    fc2: FusedBiasLinear,
    act: Activation,
}

impl VisionMlp {
    fn new(dim: usize, hidden_dim: usize, act: Activation, vb: VarBuilder) -> Result<Self> {
        Ok(Self {
            fc1: candle_nn::linear(dim, hidden_dim, vb.pp("fc1"))?.try_into()?,
            fc2: candle_nn::linear(hidden_dim, dim, vb.pp("fc2"))?.try_into()?,
            act,
        })
    }

    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        self.fc2
            .forward(&self.act.forward(&self.fc1.forward(&xs.unsqueeze(0)?)?)?)?
            .squeeze(0)
    }
}

fn rotate_half(xs: &Tensor) -> Result<Tensor> {
    let last_dim = xs.dim(D::Minus1)?;
    let xs1 = xs.narrow(D::Minus1, 0, last_dim / 2)?;
    let xs2 = xs.narrow(D::Minus1, last_dim / 2, last_dim - last_dim / 2)?;
    Tensor::cat(&[&xs2.neg()?, &xs1], D::Minus1)
}

fn apply_rotary_pos_emb_vision(xs: &Tensor, freqs: &Tensor) -> Result<Tensor> {
    let orig_ty = xs.dtype();
    let xs = xs.to_dtype(DType::F32)?;
    let cos = freqs
        .cos()?
        .unsqueeze(1)?
        .repeat((1, 1, 2))?
        .unsqueeze(0)?
        .to_dtype(DType::F32)?;
    let sin = freqs
        .sin()?
        .unsqueeze(1)?
        .repeat((1, 1, 2))?
        .unsqueeze(0)?
        .to_dtype(DType::F32)?;
    // ((&xs * cos) + (rotate_half(&xs)? * sin)?)?.to_dtype(orig_ty)
    (xs.broadcast_mul(&cos)? + rotate_half(&xs)?.broadcast_mul(&sin))?.to_dtype(orig_ty)
}

// https://github.com/huggingface/transformers/blob/a769ed45e17c44fd17b85c025863c4e4f2f73634/src/transformers/models/qwen2_vl/modeling_qwen2_vl.py#L325
struct VisionAttention {
    qkv: FusedBiasLinear,
    proj: Linear,
    num_heads: usize,
    head_dim: usize,
}

impl VisionAttention {
    fn new(dim: usize, num_heads: usize, vb: VarBuilder) -> Result<Self> {
        Ok(Self {
            qkv: candle_nn::linear(dim, dim * 3, vb.pp("qkv"))?.try_into()?,
            proj: candle_nn::linear_no_bias(dim, dim, vb.pp("proj"))?,
            num_heads,
            head_dim: dim / num_heads,
        })
    }
    fn forward(&self, xs: &Tensor, cu_seqlens: &[u32], rotary_pos_emb: &Tensor) -> Result<Tensor> {
        let seq_len = xs.dim(0)?;
        let (q, k, v) = {
            let qkv = self
                .qkv
                .forward(&xs.unsqueeze(0)?)?
                .reshape((seq_len, 3, self.num_heads, ()))?
                .permute((1, 0, 2, 3))?
                .chunk(3, 0)?;
            (qkv[0].squeeze(0)?, qkv[1].squeeze(0)?, qkv[2].squeeze(0)?)
        };

        let q = apply_rotary_pos_emb_vision(&q.unsqueeze(0)?, rotary_pos_emb)?.squeeze(0)?;
        let k = apply_rotary_pos_emb_vision(&k.unsqueeze(0)?, rotary_pos_emb)?.squeeze(0)?;

        let mut attention_mask =
            Tensor::full(f32::MIN, (1, seq_len, seq_len), q.device())?.to_dtype(q.dtype())?;
        for i in 1..cu_seqlens.len() {
            let a = cu_seqlens[i - 1] as usize;
            let b = cu_seqlens[i] as usize;
            attention_mask = attention_mask.slice_assign(
                &[&.., &(a..b), &(a..b)],
                &Tensor::zeros((1, b - a, b - a), q.dtype(), q.device())?,
            )?;
        }

        let q = q.transpose(0, 1)?;
        let k = k.transpose(0, 1)?;
        let v = v.transpose(0, 1)?;

        let att = {
            let att = (q
                .contiguous()?
                .to_dtype(DType::F32)?
                .matmul(&k.transpose(1, 2)?.contiguous()?.to_dtype(DType::F32)?)?
                / (self.head_dim as f64).sqrt())?;
            let att = att.broadcast_add(&attention_mask.to_dtype(DType::F32)?)?;
            let att = candle_nn::ops::softmax_last_dim(&att)?;
            att.matmul(&v.contiguous()?.to_dtype(DType::F32)?)?
                .transpose(0, 1)?
                .reshape((seq_len, ()))?
                .to_dtype(xs.dtype())?
        };

        self.proj.forward(&att)
    }
}

// https://github.com/huggingface/transformers/blob/f2c388e3f946862f657acc1e21b272ec946fc66c/src/transformers/models/qwen2_vl/modeling_qwen2_vl.py#L418
struct VisionBlock {
    norm1: LayerNorm,
    norm2: LayerNorm,
    mlp: VisionMlp,
    attn: VisionAttention,
}

impl VisionBlock {
    fn new(cfg: &VisionConfig, vb: VarBuilder) -> Result<Self> {
        let norm1 = layer_norm(cfg.embed_dim, 1e-6, vb.pp("norm1"))?;
        let norm2 = layer_norm(cfg.embed_dim, 1e-6, vb.pp("norm2"))?;

        let mlp_hidden_dim = (cfg.embed_dim as f64 * cfg.mlp_ratio) as usize;
        let mlp = VisionMlp::new(cfg.embed_dim, mlp_hidden_dim, cfg.hidden_act, vb.pp("mlp"))?;
        let attn = VisionAttention::new(cfg.embed_dim, cfg.num_heads, vb.pp("attn"))?;

        Ok(Self {
            norm1,
            norm2,
            mlp,
            attn,
        })
    }

    fn forward(&self, xs: &Tensor, cu_seqlens: &[u32], rotary_pos_emb: &Tensor) -> Result<Tensor> {
        let xs = (xs
            + self
                .attn
                .forward(&self.norm1.forward(xs)?, cu_seqlens, rotary_pos_emb)?)?;
        &xs + self.mlp.forward(&self.norm2.forward(&xs)?)?
    }
}

struct PatchMerger {
    ln_q: LayerNorm,
    mlp0: Linear,
    mlp2: Linear,
    hidden_size: usize,
}

impl PatchMerger {
    pub fn new(
        dim: usize,
        context_dim: usize,
        spatial_merge_size: usize,
        vb: VarBuilder,
    ) -> Result<Self> {
        let hidden_size = context_dim * spatial_merge_size.pow(2);
        let mlp0 = candle_nn::linear_no_bias(hidden_size, hidden_size, vb.pp("mlp.0"))?;
        let mlp2 = candle_nn::linear_no_bias(hidden_size, dim, vb.pp("mlp.2"))?;
        Ok(Self {
            ln_q: layer_norm(context_dim, 1e-6, vb.pp("ln_q"))?,
            mlp0,
            mlp2,
            hidden_size,
        })
    }

    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        xs.apply(&self.ln_q)?
            .reshape(((), self.hidden_size))?
            .apply(&self.mlp0)?
            .gelu()?
            .apply(&self.mlp2)
    }
}

struct VisionRotaryEmbedding {
    inv_freq: Tensor,
}

impl VisionRotaryEmbedding {
    const THETA: f32 = 10000.;

    fn new(dim: usize, device: &Device) -> Result<Self> {
        let inv_freq = (0..dim)
            .step_by(2)
            .map(|i| 1f32 / Self::THETA.powf(i as f32 / dim as f32))
            .collect::<Vec<_>>();
        let inv_freq_len = inv_freq.len();
        Ok(Self {
            inv_freq: Tensor::from_vec(inv_freq, (1, inv_freq_len), device)?,
        })
    }

    fn make_embeds(&self, seqlen: usize) -> Result<Tensor> {
        let seq =
            Tensor::arange(0f32, seqlen as f32, self.inv_freq.device())?.unsqueeze(D::Minus1)?;
        seq.broadcast_matmul(&self.inv_freq)
    }
}

pub struct Qwen2VLVisionModel {
    blocks: Vec<VisionBlock>,
    patch_merger: PatchMerger,
    patch_embed: PatchEmbed,
    rotary_pos_emb: VisionRotaryEmbedding,
    spatial_merge_size: usize,
}

impl Qwen2VLVisionModel {
    pub fn new(cfg: &VisionConfig, vb: VarBuilder) -> Result<Self> {
        let mut blocks = Vec::new();
        for i in 0..cfg.depth {
            blocks.push(VisionBlock::new(cfg, vb.pp(format!("blocks.{i}")))?);
        }

        let patch_merger = PatchMerger::new(
            cfg.hidden_size,
            cfg.embed_dim,
            cfg.spatial_merge_size,
            vb.pp("merger"),
        )?;

        let patch_embed = PatchEmbed::new(cfg, vb.pp("patch_embed"))?;

        let head_dim = cfg.embed_dim / cfg.num_heads;
        let rotary_pos_emb = VisionRotaryEmbedding::new(head_dim / 2, vb.device())?;

        Ok(Self {
            blocks,
            patch_embed,
            patch_merger,
            rotary_pos_emb,
            spatial_merge_size: cfg.spatial_merge_size,
        })
    }

    fn rot_pos_emb(&self, grid_thw: &Tensor) -> Result<Tensor> {
        let mut pos_ids = Vec::new();
        for i_thw in grid_thw.to_vec2::<u32>()? {
            let (t, h, w) = (i_thw[0], i_thw[1], i_thw[2]);
            let mut hpos_ids = Tensor::arange(0, h, grid_thw.device())?
                .unsqueeze(1)?
                .repeat((1, w as usize))?;
            hpos_ids = hpos_ids.reshape((
                h as usize / self.spatial_merge_size,
                self.spatial_merge_size,
                w as usize / self.spatial_merge_size,
                self.spatial_merge_size,
            ))?;
            hpos_ids = hpos_ids.permute((0, 2, 1, 3))?;
            hpos_ids = hpos_ids.flatten_all()?;

            let mut wpos_ids = Tensor::arange(0, w, grid_thw.device())?
                .unsqueeze(0)?
                .repeat((h as usize, 1))?;
            wpos_ids = wpos_ids.reshape((
                h as usize / self.spatial_merge_size,
                self.spatial_merge_size,
                w as usize / self.spatial_merge_size,
                self.spatial_merge_size,
            ))?;
            wpos_ids = wpos_ids.permute((0, 2, 1, 3))?;
            wpos_ids = wpos_ids.flatten_all()?;

            pos_ids.push(Tensor::stack(&[hpos_ids, wpos_ids], D::Minus1)?.repeat((t as usize, 1))?);
        }
        let pos_ids = Tensor::cat(&pos_ids, 0)?;
        let max_grid_size = grid_thw.i((.., 1..))?.max(0)?.max(0)?.to_scalar::<u32>()?;
        let rotary_pos_emb_full = self.rotary_pos_emb.make_embeds(max_grid_size as usize)?;

        assert_eq!(pos_ids.rank(), 2);
        rotary_pos_emb_full
            .index_select(&pos_ids.flatten_all()?, 0)?
            .reshape((pos_ids.dim(0)?, pos_ids.dim(1)?, ()))?
            .flatten_from(1)
    }

    pub fn forward(&self, xs: &Tensor, grid_thw: &Tensor) -> Result<Tensor> {
        let mut xs = self
            .patch_embed
            .forward(&xs.to_dtype(self.patch_merger.mlp0.weight().dtype())?)?;
        let rotary_pos_emb = self.rot_pos_emb(grid_thw)?;

        let grid_thw = grid_thw.to_device(&Device::Cpu)?;
        let cu_seqlens = (grid_thw.i((.., 1))? * grid_thw.i((.., 2))?)?
            .repeat_interleave_flat(grid_thw.i((.., 0))?.to_vec1::<u32>()?)?
            .to_dtype(DType::F32)?
            .cumsum(0)?
            .to_dtype(DType::U32)?
            .pad_with_zeros(0, 1, 0)?
            .to_vec1::<u32>()?;

        for blk in &self.blocks {
            xs = blk.forward(&xs, &cu_seqlens, &rotary_pos_emb)?;
        }

        self.patch_merger.forward(&xs)
    }
}
