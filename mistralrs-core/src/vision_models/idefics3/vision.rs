use candle_core::{DType, Device, IndexOp, Result, Tensor, D};
use candle_nn::{
    conv2d, embedding, layer_norm, Conv2d, Conv2dConfig, Embedding, LayerNorm, Linear, Module,
    VarBuilder,
};
use std::ops::Mul;

use crate::{
    layers::{Activation, CausalMasker},
    utils::unvarbuilder::UnVarBuilder,
};

use super::config::{Idefics3Config, Idefics3VisionConfig};

pub(crate) struct Idefics3SimpleMLP {
    pub(crate) proj: Linear,
}

impl Idefics3SimpleMLP {
    pub fn new(cfg: &Idefics3Config, vb: VarBuilder) -> Result<Self> {
        let in_dim = cfg.vision_config.hidden_size * cfg.scale_factor.pow(2);
        let out_dim = cfg.text_config.hidden_size;
        Ok(Self {
            proj: candle_nn::linear_no_bias(in_dim, out_dim, vb.pp("proj"))?,
        })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        x.apply(&self.proj)
    }
}

pub struct Idefics3Connector {
    scale_factor: usize,
    pub(crate) modality_projection: Idefics3SimpleMLP,
}

impl Idefics3Connector {
    pub fn new(cfg: &Idefics3Config, vb: VarBuilder) -> Result<Self> {
        Ok(Self {
            scale_factor: cfg.scale_factor,
            modality_projection: Idefics3SimpleMLP::new(cfg, vb.pp("modality_projection"))?,
        })
    }

    pub fn pixel_shuffle(&self, x: &Tensor, scale_factor: usize) -> Result<Tensor> {
        let (bs, seq, embed_dim) = x.dims3()?;
        let height = (seq as f32).sqrt() as usize;
        let width = height;
        let mut x = x.reshape((bs, height, width, embed_dim))?;
        x = x.reshape((bs, height, width / scale_factor, embed_dim * scale_factor))?;
        x = x.permute((0, 2, 1, 3))?;
        x = x.reshape((
            bs,
            width / scale_factor,
            height / scale_factor,
            embed_dim * scale_factor.pow(2),
        ))?;
        x = x.permute((0, 2, 1, 3))?;
        x.reshape((
            bs,
            (seq as f32 / scale_factor.pow(2) as f32) as usize,
            embed_dim * scale_factor.pow(2),
        ))
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let image_hidden_states = self.pixel_shuffle(x, self.scale_factor)?;
        self.modality_projection.forward(&image_hidden_states)
    }
}

struct VisionEmbeddings {
    patch_size: usize,
    patch_embedding: Conv2d,
    num_patches_per_side: usize,
    position_embedding: Embedding,
}

/// torch.bucketize with right=True
/// Returns a 1d tensor of shape (xs.len(),) on the CPU
fn bucketize_right(xs: &[f32], boundaries: &[f32], device: &Device) -> Result<Tensor> {
    // Initialize a vector to store the bucket indices
    let mut indices = vec![0; xs.len()];

    // Iterate over each element in `xs`
    for (i, &x) in xs.iter().enumerate() {
        // Find the index of the bucket for the current element
        let mut index = 0;
        for (j, &boundary) in boundaries.iter().enumerate() {
            if x < boundary {
                index = j;
                break;
            }
        }
        // If the value is greater than or equal to all boundaries, set the index to the length of boundaries
        if index == 0 && x >= boundaries[boundaries.len() - 1] {
            index = boundaries.len();
        }
        indices[i] = index as u32;
    }

    Tensor::from_vec(indices, (xs.len(),), device)
}

impl VisionEmbeddings {
    fn new(config: &Idefics3VisionConfig, vb: VarBuilder) -> Result<Self> {
        let conv_config = Conv2dConfig {
            stride: config.patch_size,
            ..Default::default()
        };
        let patch_embedding = conv2d(
            config.num_channels,
            config.hidden_size,
            config.patch_size,
            conv_config,
            vb.pp("patch_embedding"),
        )?;
        let num_patches_per_side = config.image_size / config.patch_size;
        let num_patches = num_patches_per_side.pow(2);
        Ok(Self {
            patch_size: config.patch_size,
            patch_embedding,
            num_patches_per_side,
            position_embedding: embedding(
                num_patches,
                config.hidden_size,
                vb.pp("position_embedding"),
            )?,
        })
    }

    fn forward(&self, pixel_values: &Tensor, patch_attention_mask: &Tensor) -> Result<Tensor> {
        let (bs, _, max_im_h, max_im_w) = pixel_values.dims4()?;

        let patch_embeds = self.patch_embedding.forward(pixel_values)?;

        let embeddings = patch_embeds.flatten(2, D::Minus1)?.transpose(1, 2)?;

        let (max_nb_patches_h, max_nb_patches_w) =
            (max_im_h / self.patch_size, max_im_w / self.patch_size);
        let boundaries = Tensor::arange_step(
            1.0 / self.num_patches_per_side as f32,
            1.0,
            1.0 / self.num_patches_per_side as f32,
            pixel_values.device(),
        )?
        .to_vec1::<f32>()?;
        let position_ids = Tensor::full(
            0u32,
            (bs, max_nb_patches_h * max_nb_patches_w),
            pixel_values.device(),
        )?;

        let mut new_position_ids = Vec::new();
        for (b_idx, p_attn_mask) in patch_attention_mask.chunk(bs, 0)?.iter().enumerate() {
            let p_attn_mask = p_attn_mask.squeeze(0)?;
            let nb_patches_h = p_attn_mask.i((.., 0))?.sum_all()?;
            let nb_patches_w = p_attn_mask.i((0,))?.sum_all()?;

            let fractional_coords_h = Tensor::arange_step(
                0.0,
                1.0 - 1e-6,
                1.0 / nb_patches_h.to_dtype(DType::F32)?.to_scalar::<f32>()?,
                pixel_values.device(),
            )?
            .to_vec1::<f32>()?;
            let fractional_coords_w = Tensor::arange_step(
                0.0,
                1.0 - 1e-6,
                1.0 / nb_patches_w.to_dtype(DType::F32)?.to_scalar::<f32>()?,
                pixel_values.device(),
            )?
            .to_vec1::<f32>()?;

            let bucket_coords_h =
                bucketize_right(&fractional_coords_h, &boundaries, pixel_values.device())?;
            let bucket_coords_w =
                bucketize_right(&fractional_coords_w, &boundaries, pixel_values.device())?;

            let pos_ids = bucket_coords_h
                .unsqueeze(D::Minus1)?
                .mul(self.num_patches_per_side as f64)?
                .broadcast_add(&bucket_coords_w)?
                .flatten_all()?;

            let position_ids_b = position_ids.i(b_idx)?;
            new_position_ids.push(
                p_attn_mask
                    .flatten_all()?
                    .where_cond(&pos_ids, &position_ids_b)?,
            );
        }
        let position_ids = Tensor::stack(&new_position_ids, 0)?;
        let position_ids = position_ids.to_device(self.position_embedding.embeddings().device())?;
        embeddings.broadcast_add(&self.position_embedding.forward(&position_ids)?)
    }

    fn residual_tensors(&self) -> Vec<(String, Tensor)> {
        let uvb = UnVarBuilder::new();

        uvb.pp("patch_embedding").add(&self.patch_embedding);
        uvb.pp("position_embedding").add(&self.position_embedding);

        uvb.to_safetensors()
    }
}

struct Attention {
    embed_dim: usize,
    num_heads: usize,
    head_dim: usize,
    scale: f64,
    q_proj: Linear,
    k_proj: Linear,
    v_proj: Linear,
    o_proj: Linear,
    neg_inf: Tensor,
}

impl Attention {
    fn new(config: Idefics3VisionConfig, vb: VarBuilder) -> Result<Self> {
        let embed_dim = config.hidden_size;
        let num_heads = config.num_attention_heads;
        let head_dim = embed_dim / num_heads;
        let scale = 1.0 / (head_dim as f64).sqrt();

        let q_proj = candle_nn::linear(embed_dim, embed_dim, vb.pp("q_proj"))?;
        let k_proj = candle_nn::linear(embed_dim, embed_dim, vb.pp("k_proj"))?;
        let v_proj = candle_nn::linear(embed_dim, embed_dim, vb.pp("v_proj"))?;
        let o_proj = candle_nn::linear(embed_dim, embed_dim, vb.pp("out_proj"))?;

        Ok(Self {
            embed_dim,
            num_heads,
            head_dim,
            scale,
            q_proj,
            k_proj,
            v_proj,
            o_proj,
            neg_inf: Tensor::new(f32::NEG_INFINITY, vb.device())?.to_dtype(vb.dtype())?,
        })
    }

    fn forward(&self, xs: &Tensor, attention_mask: Option<&Tensor>) -> Result<Tensor> {
        let (b_sz, q_len, _) = xs.dims3()?;

        let mut q = self.q_proj.forward(xs)?;
        let mut k = self.k_proj.forward(xs)?;
        let mut v = self.v_proj.forward(xs)?;

        q = q
            .reshape((b_sz, q_len, self.num_heads, self.head_dim))?
            .transpose(1, 2)?;
        k = k
            .reshape((b_sz, q_len, self.num_heads, self.head_dim))?
            .transpose(1, 2)?;
        v = v
            .reshape((b_sz, q_len, self.num_heads, self.head_dim))?
            .transpose(1, 2)?;

        let attn_weights =
            (q.contiguous()?.matmul(&k.transpose(2, 3)?.contiguous()?)? * self.scale)?;

        let mut attn_weights = CausalMasker.apply_mask_one_and_zero(
            &attention_mask.map(|x| x.to_dtype(DType::U8).unwrap()),
            attn_weights,
            &self.neg_inf,
        )?;
        attn_weights = candle_nn::ops::softmax_last_dim(&attn_weights)?;
        let attn_output = attn_weights.matmul(&v.contiguous()?)?;

        attn_output
            .transpose(1, 2)?
            .reshape((b_sz, q_len, self.embed_dim))?
            .apply(&self.o_proj)
    }

    fn residual_tensors(&self) -> Vec<(String, Tensor)> {
        let uvb = UnVarBuilder::new();

        uvb.pp("q_proj").add(&self.q_proj);
        uvb.pp("k_proj").add(&self.k_proj);
        uvb.pp("v_proj").add(&self.v_proj);
        uvb.pp("out_proj").add(&self.o_proj);

        uvb.to_safetensors()
    }
}

struct VisionMLP {
    activation: Activation,
    fc1: Linear,
    fc2: Linear,
}

impl VisionMLP {
    fn new(config: Idefics3VisionConfig, vb: VarBuilder) -> Result<Self> {
        let fc1 = candle_nn::linear(config.hidden_size, config.intermediate_size, vb.pp("fc1"))?;
        let fc2 = candle_nn::linear(config.intermediate_size, config.hidden_size, vb.pp("fc2"))?;
        Ok(Self {
            activation: config.hidden_act,
            fc1,
            fc2,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let mut x = self.fc1.forward(x)?;
        x = self.activation.forward(&x)?;
        self.fc2.forward(&x)
    }

    fn residual_tensors(&self) -> Vec<(String, Tensor)> {
        let uvb = UnVarBuilder::new();

        uvb.pp("fc1").add(&self.fc1);
        uvb.pp("fc2").add(&self.fc2);

        uvb.to_safetensors()
    }
}

struct EncoderLayer {
    mlp: VisionMLP,
    attn: Attention,
    layer_norm_1: LayerNorm,
    layer_norm_2: LayerNorm,
}

impl EncoderLayer {
    fn new(config: Idefics3VisionConfig, vb: VarBuilder) -> Result<Self> {
        let mlp = VisionMLP::new(config.clone(), vb.pp("mlp"))?;
        let attn = Attention::new(config.clone(), vb.pp("self_attn"))?;
        let layer_norm_1 = layer_norm(
            config.hidden_size,
            config.layer_norm_eps,
            vb.pp("layer_norm1"),
        )?;
        let layer_norm_2 = layer_norm(
            config.hidden_size,
            config.layer_norm_eps,
            vb.pp("layer_norm2"),
        )?;
        Ok(Self {
            mlp,
            attn,
            layer_norm_1,
            layer_norm_2,
        })
    }

    fn forward(&self, xs: &Tensor, attention_mask: Option<&Tensor>) -> Result<Tensor> {
        let residual = xs.clone();

        let hidden_states = self.layer_norm_1.forward(xs)?;
        let hidden_states = self.attn.forward(&hidden_states, attention_mask)?;
        let hidden_states = (hidden_states + residual)?;

        let residual = &hidden_states;
        let hidden_states = self.layer_norm_2.forward(&hidden_states)?;
        let hidden_states = self.mlp.forward(&hidden_states)?;
        hidden_states + residual
    }
}

struct Encoder {
    layers: Vec<EncoderLayer>,
}

impl Encoder {
    fn new(config: &Idefics3VisionConfig, vb: VarBuilder) -> Result<Self> {
        let mut layers = Vec::new();
        let vb_l = vb.pp("layers");
        for i in 0..config.num_hidden_layers {
            layers.push(EncoderLayer::new(config.clone(), vb_l.pp(i))?);
        }
        Ok(Self { layers })
    }

    fn forward(&self, xs: &Tensor, attention_mask: Option<&Tensor>) -> Result<Tensor> {
        let mut hidden_states = xs.clone();
        for layer in &self.layers {
            hidden_states = layer.forward(&hidden_states, attention_mask)?;
        }
        Ok(hidden_states)
    }
}

pub struct Idefics3VisionTransformer {
    embeddings: VisionEmbeddings,
    encoder: Encoder,
    post_layernorm: LayerNorm,
    patch_size: usize,
}

impl Idefics3VisionTransformer {
    pub fn new(config: &Idefics3VisionConfig, vb: VarBuilder) -> Result<Self> {
        let embeddings = VisionEmbeddings::new(config, vb.pp("embeddings"))?;
        let post_layernorm = layer_norm(
            config.hidden_size,
            config.layer_norm_eps,
            vb.pp("post_layernorm"),
        )?;
        let encoder = Encoder::new(config, vb.pp("encoder"))?;
        Ok(Self {
            embeddings,
            encoder,
            post_layernorm,
            patch_size: config.patch_size,
        })
    }

    pub fn forward(
        &self,
        pixel_values: &Tensor,
        attention_mask: Option<&Tensor>,
    ) -> Result<Tensor> {
        let bs = pixel_values.dim(0)?;
        let patch_attention_mask = if let Some(attn_mask) = attention_mask {
            attn_mask.clone()
        } else {
            Tensor::ones(
                (
                    bs,
                    pixel_values.dim(2)? / self.patch_size,
                    pixel_values.dim(3)? / self.patch_size,
                ),
                DType::U8,
                pixel_values.device(),
            )?
        };

        let hidden_states = self
            .embeddings
            .forward(pixel_values, &patch_attention_mask)?;

        let attention_mask = if attention_mask.is_none() {
            None
        } else {
            let mask = patch_attention_mask
                .reshape((patch_attention_mask.dim(0)?, ()))?
                .to_dtype(hidden_states.dtype())?;
            Some(CausalMasker.expand_mask(&mask, hidden_states.dtype(), None)?)
        };
        let hidden_states = self
            .encoder
            .forward(&hidden_states, attention_mask.as_ref())?;
        hidden_states.apply(&self.post_layernorm)
    }

    pub fn residual_tensors(&self) -> Vec<(String, Tensor)> {
        let uvb = UnVarBuilder::new();

        uvb.pp("post_layernorm").add(&self.post_layernorm);
        uvb.pp("embeddings")
            .extend(self.embeddings.residual_tensors());

        let uvb_enc = uvb.pp("encoder");
        for (i, layer) in self.encoder.layers.iter().enumerate() {
            let uvb_l = uvb_enc.pp("layers").pp(i);

            uvb_l.pp("layer_norm1").add(&layer.layer_norm_1);
            uvb_l.pp("layer_norm2").add(&layer.layer_norm_2);
            uvb_l.pp("mlp").extend(layer.mlp.residual_tensors());
            uvb_l.pp("self_attn").extend(layer.attn.residual_tensors());
        }

        uvb.to_safetensors()
    }
}
