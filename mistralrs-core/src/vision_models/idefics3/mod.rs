#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

mod config;
mod inputs_processor;
mod vision;

use candle_core::{DType, IndexOp, Result, Tensor, D};
use candle_nn::{Linear, VarBuilder};
use config::Idefics3Config;
use vision::{Idefics3Connector, Idefics3VisionTransformer};

use crate::{
    dummy_paged_attention::AttentionImplementation,
    layers::CausalMasker,
    models::llama::Llama,
    pipeline::{
        text_models_inputs_processor::{FlashParams, PagedAttentionInputMetadata},
        NormalLoadingMetadata, NormalModel,
    },
};

pub struct Idefics3Model {
    lm_head: Linear,
    text_model: Llama,
    connector: Idefics3Connector,
    vision: Idefics3VisionTransformer,
    config: Idefics3Config,
    dtype: DType,
}

impl Idefics3Model {
    pub fn new(
        cfg: &Idefics3Config,
        vb: VarBuilder,
        is_gptx: bool,
        normal_loading_metadata: NormalLoadingMetadata,
        attention_mechanism: AttentionImplementation,
    ) -> Result<Self> {
        let lm_head = candle_nn::linear_no_bias(
            cfg.text_config.hidden_size,
            cfg.text_config.vocab_size,
            vb.pp("lm_head"),
        )?;

        let vb = vb.pp("model");
        let text_model = Llama::new(
            &cfg.text_config,
            vb.pp("text_model"),
            is_gptx,
            normal_loading_metadata,
            attention_mechanism,
        )?;
        let connector = Idefics3Connector::new(cfg, vb.pp("connector"))?;
        let vision = Idefics3VisionTransformer::new(&cfg.vision_config, vb.pp("vision_model"))?;

        Ok(Self {
            lm_head,
            text_model,
            connector,
            vision,
            config: cfg.clone(),
            dtype: vb.dtype(),
        })
    }

    fn inputs_merger(
        &self,
        input_ids: &Tensor,
        input_embeds: &Tensor,
        image_hidden_states: &Tensor,
    ) -> Result<Tensor> {
        // Docs copied from Transformers impl
        /*
        This method aims at merging the token embeddings with the image hidden states into one single sequence of vectors that are fed to the transformer LM.
        The merging happens as follows:
        - The text token sequence is: `tok_1 tok_2 tok_3 <fake_token_around_image> <image> <image> ... <image> <fake_token_around_image> tok_4`.
        - We get the image hidden states for the image through the vision encoder (and potentially the perceiver), and that hidden state is then projected into the text embedding space.
        We thus have a sequence of image hidden states of size (1, image_seq_len, hidden_dim), where 1 is for batch_size of 1 image and hidden_dim is the hidden_dim of the LM transformer.
        - The merging happens so that we obtain the following sequence: `vector_tok_1 vector_tok_2 vector_tok_3 vector_fake_tok_around_image {sequence of image_seq_len image hidden states} vector_fake_toke_around_image vector_tok_4`. That sequence is fed to the LM.
        - To fit the format of that sequence, `input_ids`, `input_embeds`, `attention_mask` are all 3 adapted to insert the image hidden states.
        */
        let (_, _, vision_hidden_size) = image_hidden_states.dims3()?;
        let bs = input_ids.dim(0)?;
        let special_image_token_mask = input_ids.eq(self.config.image_token_id as f64)?;
        let mut new_inputs_embeds = input_embeds.clone();
        let reshaped_image_hidden_states =
            image_hidden_states.reshape((bs, (), vision_hidden_size))?;
        assert_eq!(input_embeds.dim(0)?, 1);
        assert_eq!(reshaped_image_hidden_states.dim(0)?, 1);
        let special_image_token_mask = special_image_token_mask.i(0)?.to_vec1::<u8>()?;
        let mut image_hidden_state_i = 0;
        for (i, v) in special_image_token_mask.iter().enumerate() {
            if *v != 0 {
                new_inputs_embeds = new_inputs_embeds.slice_assign(
                    &[&.., &i, &..],
                    &reshaped_image_hidden_states
                        .i((.., image_hidden_state_i, ..))?
                        .unsqueeze(1)?,
                )?;
                image_hidden_state_i += 1;
            }
        }
        Ok(new_inputs_embeds)
    }

    #[allow(clippy::too_many_arguments)]
    fn forward_inner(
        &self,
        input_ids: &Tensor,
        pixel_values: Option<Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
        pixel_attention_mask: Option<Tensor>,
        metadata: Option<(Vec<(Tensor, Tensor)>, &mut PagedAttentionInputMetadata)>,
        flash_params: &FlashParams,
    ) -> Result<Tensor> {
        let input_embeds = if let Some(pixel_values) = pixel_values {
            // == START VISUAL INPUTS INTEGRATION ==
            let (batch_size, num_images, _, _, _) = pixel_values.dims5()?;
            let mut s = vec![batch_size * num_images];
            s.extend(pixel_values.dims()[2..].to_vec());
            let pixel_values = pixel_values.reshape(s)?;

            // Remove padding images which are full of 0s
            let nb_values_per_image = pixel_values.dims()[1..].iter().product::<usize>();
            let real_images_inds = pixel_values
                .eq(0.0f64)?
                .sum(vec![
                    pixel_values.dims().len() - 1,
                    pixel_values.dims().len() - 2,
                    pixel_values.dims().len() - 3,
                ])?
                .ne(nb_values_per_image as f64)?;
            let mut batches = Vec::new();
            for (batch, use_it) in pixel_values
                .chunk(pixel_values.dim(0)?, 0)?
                .iter()
                .zip(real_images_inds.chunk(real_images_inds.dim(0)?, 0)?)
            {
                let use_it = use_it.squeeze(0)?.to_scalar::<u8>()? != 0;
                if use_it {
                    batches.push(batch.clone());
                }
            }
            let pixel_values = Tensor::cat(&batches, 0)?;

            // Vision attention mask
            let pixel_attention_mask = if let Some(pixel_attention_mask) = pixel_attention_mask {
                let pixel_attention_mask = pixel_attention_mask.reshape((
                    batch_size * num_images,
                    pixel_attention_mask.dims()[2],
                    pixel_attention_mask.dims()[3],
                ))?;
                let mut batches = Vec::new();
                for (batch, use_it) in pixel_attention_mask
                    .chunk(pixel_attention_mask.dim(0)?, 0)?
                    .iter()
                    .zip(real_images_inds.chunk(real_images_inds.dim(0)?, 0)?)
                {
                    let use_it = use_it.squeeze(0)?.to_scalar::<u8>()? != 0;
                    if use_it {
                        batches.push(batch.clone());
                    }
                }
                Tensor::cat(&batches, 0)?
            } else {
                Tensor::ones(
                    (
                        pixel_values.dims()[0],
                        pixel_values.dims()[2],
                        pixel_values.dims()[3],
                    ),
                    DType::U8,
                    pixel_values.device(),
                )?
            };

            let patch_size = self.config.vision_config.patch_size;
            let patches_subgrid = pixel_attention_mask.unfold(1, patch_size, patch_size)?;
            let patches_subgrid = patches_subgrid.unfold(2, patch_size, patch_size)?;

            let patch_attention_mask = patches_subgrid
                .sum((D::Minus1, D::Minus2))?
                .gt(0.0)?
                .to_dtype(DType::U8)?;

            let pixel_values = pixel_values.to_dtype(self.dtype)?;

            // Get seq from vision encoder
            let image_hidden_states = self
                .vision
                .forward(&pixel_values, Some(&patch_attention_mask))?;

            // Modality proj and perceiver resampling
            let image_hidden_states = self.connector.forward(&image_hidden_states)?;

            if CausalMasker.calculate_past_kv_len(&self.text_model.cache().full().lock())? == 0 {
                self.inputs_merger(
                    input_ids,
                    &self.text_model.get_input_embeddings(input_ids)?,
                    &image_hidden_states,
                )?
            } else {
                candle_core::bail!("Pixel values were specified for a non-prompt.")
            }
        } else {
            self.text_model.get_input_embeddings(input_ids)?
        };

        self.text_model.forward_embeds(
            input_ids,
            input_embeds,
            seqlen_offsets,
            start_offsets_kernel,
            context_lens,
            metadata,
            flash_params,
        )
    }
}
