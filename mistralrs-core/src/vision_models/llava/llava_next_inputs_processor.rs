#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
use std::any::Any;
use std::sync::Arc;

use candle_core::Result;
use candle_core::{DType, Device, Tensor};
use image::GenericImageView;
use itertools::Itertools;
use regex_automata::meta::Regex;
use tokenizers::Tokenizer;

use crate::pipeline::text_models_inputs_processor::{
    get_completion_input, get_prompt_input, PagedAttentionMeta,
};
use crate::pipeline::{
    text_models_inputs_processor, InputsProcessor, InputsProcessorType, MessagesAction, Processor,
};
use crate::sequence::Sequence;
use crate::vision_models::image_processor::{self, ImagePreProcessor, PreprocessedImages};
use crate::vision_models::llava::config::Config as LLaVANextConfig;
use crate::vision_models::preprocessor_config::{PreProcessorConfig, ToFilter};
use crate::vision_models::{preprocessor_config, ModelInputs};

use super::llava_next::LLaVANextVisionSpecificArgs;
use super::utils::{
    calculate_unpad, divide_to_samples, get_anyres_image_grid_shape, get_num_samples,
    resize_and_pad_image, select_best_resolution, LLaVAImageProcessor,
};

pub struct LLaVANextProcessor {
    inputs_processor: Arc<LLaVANextInputProcessor>,
}

impl Processor for LLaVANextProcessor {
    fn inputs_processor(&self) -> Arc<dyn InputsProcessor> {
        self.inputs_processor.clone()
    }
    fn get_special_tokens(&self) -> &[&'static str] {
        &[]
    }
    fn template_action(&self) -> MessagesAction {
        MessagesAction::FlattenOnlyText
    }
}

impl LLaVANextProcessor {
    pub fn new(config: &str) -> Self {
        let model_config =
            serde_json::from_str::<LLaVANextConfig>(config).expect("Failed to parse model config.");
        let image_tag_splitter = Regex::new(r"<image>").expect("Failed to compile split regex.");
        let inputs_processor = Arc::new(LLaVANextInputProcessor {
            image_tag_splitter,
            model_config: model_config.clone(),
        });
        Self { inputs_processor }
    }
}

pub struct LLaVANextInputProcessor {
    image_tag_splitter: Regex,
    model_config: LLaVANextConfig,
}

impl LLaVANextInputProcessor {
    fn get_num_image_tokens(&self, image_size: (u32, u32)) -> usize {
        let patch_size = self.model_config.vision_config.patch_size;
        let image_grid_pinpoints = self.model_config.image_grid_pinpoints.clone().unwrap();
        let anyres_grid_shape =
            get_anyres_image_grid_shape(image_size, &image_grid_pinpoints, patch_size as u32);
        let patch_per_side = self.model_config.vision_config.image_size / patch_size;
        let unpad_shape = calculate_unpad(anyres_grid_shape, image_size);
        patch_per_side * patch_per_side + (unpad_shape.0 as usize + 1) * (unpad_shape.1 as usize)
    }
}

// Copy from phi3_inputs_processor. different is (1) calculate of num_image_token (2) process_anyres_image (3)image_ids_pad
impl InputsProcessor for LLaVANextInputProcessor {
    fn get_type(&self) -> InputsProcessorType {
        InputsProcessorType::Vision
    }
    fn process_inputs(
        &self,
        tokenizer: Arc<Tokenizer>,
        input_seqs: &mut [&mut Sequence],
        is_prompt: bool,
        is_xlora: bool,
        device: &Device,
        no_kv_cache: bool,
        last_n_context_len: Option<(usize, usize)>,
        other_config: Option<Arc<dyn Any>>,
        mut paged_attn_metadata: Option<PagedAttentionMeta<'_>>,
    ) -> anyhow::Result<Box<dyn Any>> {
        if is_xlora {
            anyhow::bail!("Cannot make inputs for X-LoRA vision model.");
        }
        if no_kv_cache {
            anyhow::bail!("Vision model must have kv cache.");
        }

        let config = other_config
            .clone()
            .expect("Need a PreProcessorConfig config.");
        let config: &PreProcessorConfig = config.downcast_ref().expect("Downcast failed.");
        let crop_size = (
            *config.crop_size.as_ref().unwrap().get("width").unwrap(),
            *config.crop_size.as_ref().unwrap().get("height").unwrap(),
        );
        let (pixel_values, image_sizes, num_img_tokens, num_image_samples) = if is_prompt
            && input_seqs
                .iter()
                .map(|seq| seq.images().is_some())
                .all(|x| x)
        {
            let mut pixel_values_accum = Vec::new();
            let mut image_sizes_accum = Vec::new();
            let mut num_img_tokens_accum = Vec::new();
            let mut num_image_samples_accum = Vec::new();
            for seq in input_seqs.iter_mut() {
                let imgs = seq
                    .take_images()
                    .expect("Need to have images by this point.");
                let PreprocessedImages {
                    pixel_values,
                    pixel_attention_mask: _,
                    image_sizes,
                    num_img_tokens,
                } = self.preprocess(imgs.clone(), config, device)?;
                let image_sizes = image_sizes.unwrap();
                pixel_values_accum.push(pixel_values);
                image_sizes_accum.push(image_sizes);
                num_img_tokens_accum.push(num_img_tokens.unwrap());
                let image_grid_pinpoints = self.model_config.image_grid_pinpoints.clone().unwrap();
                let num_img_samples = imgs
                    .iter()
                    .map(|img| {
                        let original_size = img.dimensions();
                        get_num_samples(original_size, &image_grid_pinpoints, crop_size) as usize
                    })
                    .collect::<Vec<_>>();
                num_image_samples_accum.push(num_img_samples);
            }
            (
                Some(Tensor::cat(&pixel_values_accum, 0)?),
                Some(image_sizes_accum),
                Some(num_img_tokens_accum),
                Some(num_image_samples_accum),
            )
        } else {
            let text_models_inputs_processor::ModelInputs {
                input_ids,
                input_ids_full: _,
                seqlen_offsets,
                seqlen_offsets_full: _,
                seqlen_offsets_kernel,
                seqlen_offsets_kernel_full: _,
                context_lens,
                position_ids,
                paged_attn_meta,
            } = *text_models_inputs_processor::TextInputsProcessor
                .process_inputs(
                    tokenizer,
                    input_seqs,
                    is_prompt,
                    is_xlora,
                    device,
                    no_kv_cache,
                    last_n_context_len,
                    other_config,
                    paged_attn_metadata,
                )?
                .downcast::<text_models_inputs_processor::ModelInputs>()
                .expect("Downcast failed.");
            return Ok(Box::new(ModelInputs {
                input_ids,
                seqlen_offsets,
                seqlen_offsets_kernel,
                context_lens,
                position_ids,
                pixel_values: None,
                model_specific_args: Box::new(LLaVANextVisionSpecificArgs {
                    image_sizes: None,
                    num_image_tokens: None,
                    num_image_samples: None,
                }),
                paged_attn_meta,
            }));
        };

        let num_image_tokens_flat = num_img_tokens
            .clone()
            .unwrap()
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        let num_image_samples = num_image_samples
            .unwrap()
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();

        let mut toks = Vec::new();
        let detokenized = tokenizer
            .decode_batch(
                &input_seqs
                    .iter()
                    .map(|seq| seq.get_toks())
                    .collect::<Vec<_>>(),
                false,
            )
            .map_err(anyhow::Error::msg)?;

        for (detokenized, (seq, num_img_tokens)) in detokenized.into_iter().zip(
            input_seqs
                .iter_mut()
                .zip(num_img_tokens.unwrap().into_iter()),
        ) {
            let splits = self
                .image_tag_splitter
                .split(&detokenized)
                .map(|span| &detokenized[span.range()])
                .collect::<Vec<_>>();
            let prompt_chunks = splits
                .iter()
                .map(|s| {
                    // we don't use encode_batch here, because encode_batch will pad 0 to the end of the shor sequences, which will cause the image_ids_pad to be wrong.
                    tokenizer
                        .encode(*s, true)
                        .unwrap()
                        .get_ids()
                        .to_vec()
                        .iter()
                        .map(|x| *x as i64)
                        .collect()
                })
                .collect::<Vec<Vec<_>>>();
            let mut image_ids_pad = Vec::new();
            for (i, num_img_token) in num_img_tokens.iter().enumerate() {
                let mut image_id_pad = vec![0; *num_img_token];
                image_id_pad[0] = -(i as i64 + 1);
                image_ids_pad.push(image_id_pad);
            }
            let mut input_ids: Vec<i64> = Vec::new();
            for item in prompt_chunks
                .iter()
                .map(|x| x.to_vec())
                .interleave(image_ids_pad)
            {
                input_ids.extend(item);
            }
            // NOTE(EricLBuehler): Casting to u32 is fine, we don't care about the other toks
            seq.set_toks(
                input_ids
                    .iter()
                    .map(|x| if *x < 0 { 0u32 } else { *x as u32 })
                    .collect::<Vec<_>>(),
            );
            if let Some(ref mut metadata) = paged_attn_metadata {
                // Free and then reallocate as appropriate
                metadata.block_engine.free_sequence(*seq.id());
                metadata.block_engine.allocate(*seq);
            }

            toks.push(input_ids);
        }

        let text_models_inputs_processor::InputMetadata {
            input,
            positions,
            positions_kernel,
            context_lens,
            position_ids,
            paged_attn_meta,
        } = if is_prompt {
            get_prompt_input(
                toks,
                input_seqs,
                device,
                last_n_context_len,
                paged_attn_metadata.as_mut(),
            )?
        } else {
            get_completion_input(
                toks,
                input_seqs,
                device,
                no_kv_cache,
                last_n_context_len,
                paged_attn_metadata.as_mut(),
            )?
        };
        Ok(Box::new(ModelInputs {
            input_ids: input,
            seqlen_offsets: positions,
            seqlen_offsets_kernel: positions_kernel,
            context_lens,
            position_ids,
            pixel_values,
            model_specific_args: Box::new(LLaVANextVisionSpecificArgs {
                image_sizes,
                num_image_tokens: Some(num_image_tokens_flat),
                num_image_samples: Some(num_image_samples),
            }),
            paged_attn_meta,
        }))
    }
}

impl ImagePreProcessor for LLaVANextInputProcessor {
    #[allow(clippy::excessive_precision)]
    const DEFAULT_MEAN: [f64; 3] = [0.48145466, 0.4578275, 0.40821073];
    #[allow(clippy::excessive_precision)]
    const DEFAULT_STD: [f64; 3] = [0.26862954, 0.26130258, 0.27577711];
    fn preprocess(
        &self,
        images: Vec<image::DynamicImage>,
        config: &preprocessor_config::PreProcessorConfig,
        device: &candle_core::Device,
    ) -> candle_core::Result<image_processor::PreprocessedImages> {
        if images.len() > 1 {
            candle_core::bail!("Can only process one image per batch"); // This is no different from phi3_input_processor
        };
        let resized_size = *config.size.as_ref().unwrap().get("shortest_edge").unwrap() as usize;
        let image = images[0].clone();
        let original_size = image.dimensions();
        let image_grid_pinpoints = self.model_config.image_grid_pinpoints.clone().unwrap();
        let best_resolution = select_best_resolution(original_size, &image_grid_pinpoints);
        // Here I didn't use mistral_vision::Transform, because a lot transformations are before turning the image into a tensor
        let image_padded = resize_and_pad_image(&image, best_resolution);
        let filter = config.resampling.to_filter()?;
        let image_original_resize =
            image.resize_exact(resized_size as u32, resized_size as u32, filter);
        let mut samples = vec![image_original_resize];
        for patch in divide_to_samples(
            &image_padded,
            (
                *config.crop_size.as_ref().unwrap().get("width").unwrap(),
                *config.crop_size.as_ref().unwrap().get("height").unwrap(),
            ),
        ) {
            samples.push(patch);
        }
        let image_mean = config
            .image_mean
            .unwrap_or(Self::DEFAULT_MEAN)
            .map(|x| x as f32);
        let image_std = config
            .image_std
            .unwrap_or(Self::DEFAULT_STD)
            .map(|x| x as f32);
        let pixel_values = samples
            .iter()
            .map(|x| {
                LLaVAImageProcessor::process_one_image(
                    x,
                    config,
                    resized_size as u32,
                    filter,
                    DType::BF16,
                    device,
                    &image_mean,
                    &image_std,
                )
            })
            .collect::<Result<Vec<Tensor>>>()?;
        let pixel_values = Tensor::stack(&pixel_values, 0)?;

        Ok(image_processor::PreprocessedImages {
            pixel_values,
            pixel_attention_mask: None,
            image_sizes: Some((original_size.0 as usize, original_size.1 as usize)),
            num_img_tokens: Some(vec![self.get_num_image_tokens(original_size)]),
        })
    }
}
