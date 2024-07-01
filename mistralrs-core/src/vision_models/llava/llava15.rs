#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_arguments
)]
use super::llava_llm::{LLaVALLM, Llama, Mistral};
use crate::device_map::DeviceMapper;
use crate::ops::NonZeroOp;
use crate::pipeline::IsqModel;
use crate::pipeline::NormalLoadingMetadata;
use crate::pipeline::VisionModel;
use crate::vision_models::clip::{ClipConfig, ClipVisionTransformer};
use crate::vision_models::llava::config::Config;
use candle_core::quantized::QMatMul;
use candle_core::{bail, DType, Device, IndexOp, Result, Tensor};
use candle_nn::{linear, Activation, Linear, VarBuilder};

pub(crate) struct LLaVAVisionSpecificArgs; // only a dumb struct to satisfy the trait

pub struct MMProjector {
    linear_1: Linear,
    activation: Activation,
    linear_2: Linear,
}

impl MMProjector {
    pub fn new(vb: &VarBuilder, config: &Config) -> Result<Self> {
        let linear_1 = linear(
            config.vision_config.hidden_size,
            config.text_config.hidden_size,
            vb.pp("multi_modal_projector.linear_1"),
        )?;
        let activation = match config.projector_hidden_act.as_str() {
            "gelu" => Activation::Gelu,
            _ => {
                bail!(
                    "Unsupporg projector hidden act: {}",
                    config.projector_hidden_act
                );
            }
        };
        let linear_2 = linear(
            config.text_config.hidden_size,
            config.text_config.hidden_size,
            vb.pp("multi_modal_projector.linear_2"),
        )?;
        Ok(Self {
            linear_1,
            activation,
            linear_2,
        })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        x.apply(&self.linear_1)?
            .apply(&self.activation)?
            .apply(&self.linear_2)
    }
}

pub struct ClipVisionTower {
    model: ClipVisionTransformer,
    select_layer: isize,
    select_feature_method: String,
    config: ClipConfig,
}

impl ClipVisionTower {
    pub fn new(
        vb: VarBuilder,
        select_layer: isize,
        select_feature_method: &str,
        config: &ClipConfig,
    ) -> Result<Self> {
        let model = ClipVisionTransformer::new(vb, config)?;
        Ok(Self {
            model,
            select_layer,
            select_feature_method: select_feature_method.to_string(),
            config: config.clone(),
        })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let result = self.model.forward_get_hidden_states(x)?;
        let index = result.len() as isize + self.select_layer;
        let result = result[index as usize].clone();
        if self.select_feature_method == "cls_patch" || self.select_feature_method == "full" {
            Ok(result)
        } else {
            result.i((.., 1..))
        }
    }

    pub fn num_patches_per_side(&self) -> usize {
        self.config.image_size / self.config.patch_size
    }
}

pub struct Model {
    clip_vision_tower: ClipVisionTower,
    mm_projector: MMProjector,
    llm: Box<dyn LLaVALLM>,
    config: Config,
    device: Device,
}

impl Model {
    pub fn new(
        config: &Config,
        vb: VarBuilder,
        is_gptx: bool,
        normal_loading_metadata: NormalLoadingMetadata,
    ) -> Result<Self> {
        let device = normal_loading_metadata.real_device.clone();

        let clip_config = config.to_clip_config();
        let mm_projector = MMProjector::new(&vb, config)?;
        let clip_vision_tower = ClipVisionTower::new(
            vb.pp("vision_tower.vision_model"),
            config.vision_feature_layer,
            &config.vision_feature_select_strategy,
            &clip_config,
        )?;

        let llm: Box<dyn LLaVALLM> = match config.text_config.model_type.as_str() {
            "llama" => {
                let llama_config = config.to_llama_config();
                let llama = Llama::new(
                    &llama_config,
                    vb.pp("language_model"),
                    is_gptx,
                    normal_loading_metadata,
                )?;
                Box::new(llama)
            }
            "mistral" => {
                let mistral_config = config.to_mistral_config();
                let mistral = Mistral::new(
                    &mistral_config,
                    vb.pp("language_model"),
                    is_gptx,
                    normal_loading_metadata,
                )?;
                Box::new(mistral)
            }
            _ => {
                bail!("Unsupported model type: {}", config.text_config.model_type);
            }
        };
        Ok(Self {
            clip_vision_tower,
            mm_projector,
            llm,
            config: config.clone(),
            device,
        })
    }

    pub fn encode_images(&self, x: &Tensor) -> Result<Tensor> {
        let mut image_features = self.clip_vision_tower.forward(x)?;
        image_features = self.mm_projector.forward(&image_features)?;
        Ok(image_features)
    }

    pub fn prepare_inputs_labels_for_multimodal(
        &self,
        input_ids: &Tensor, //[1,seq_len]
        images: &Tensor,    //[sum of samples of all images,chanel,width,height]
        num_image_tokens: usize,
    ) -> Result<Tensor> {
        let image_indexes = input_ids
            .squeeze(0)?
            .lt(0i64)?
            .nonzero()?
            .squeeze(1)?
            .to_vec1::<u32>()?;
        let image_ids = image_indexes
            .iter()
            .map(|x| Ok(-(input_ids.i((0, *x as usize))?.to_scalar::<i64>()?)))
            .collect::<Result<Vec<i64>>>()?;
        let mut result = input_ids.clamp(0i64, i64::MAX)?.to_dtype(DType::U32)?;
        result = self.llm.embed(&result)?; //[seq_len,hidden_size]
        let image_features = self.encode_images(images)?; //[num of images,patch_size*patch_size,hidden_size]
        let num_of_images = image_features.shape().dims()[0];
        let mut image_features_vec = Vec::new();
        for i in 0..num_of_images {
            image_features_vec.push(image_features.get(i)?.flatten(0, 1)?);
        }
        for (i, image_index) in image_indexes.iter().enumerate() {
            let image_id = image_ids[i];
            result = result.slice_assign(
                &[
                    &(0usize..1usize),
                    &(*image_index as usize..*image_index as usize + num_image_tokens),
                    &(..),
                ],
                &image_features_vec[(image_id - 1) as usize],
            )?;
        }
        //truncate
        let (_, seq_len) = input_ids.shape().dims2()?;
        if seq_len > self.config.text_config.max_length {
            result = result.i((.., ..self.config.text_config.max_length, ..))?
        }
        Ok(result)
    }

    pub fn forward_inputs(
        &self,
        input_ids: &Tensor,
        pixel_values: Option<Tensor>,
        num_image_tokens: Option<usize>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
        position_ids: Vec<usize>,
    ) -> Result<Tensor> {
        if let Some(ref pixel_values) = pixel_values {
            // we assume(as it should be) only prompt request contains image
            let input_embeds = self.prepare_inputs_labels_for_multimodal(
                input_ids,
                pixel_values,
                num_image_tokens.unwrap(),
            )?;
            self.llm.forward_input_embed(
                input_ids,
                input_embeds,
                seqlen_offsets,
                start_offsets_kernel,
                context_lens,
            )
        } else {
            self.llm.forward(
                input_ids,
                seqlen_offsets,
                start_offsets_kernel,
                context_lens,
                position_ids,
            )
        }
    }
}

impl IsqModel for Model {
    fn get_tensors(&mut self) -> (Vec<(&mut QMatMul, Option<usize>)>, &dyn DeviceMapper) {
        // I don't really get this part
        self.llm.get_tensors()
    }
}

impl VisionModel for Model {
    fn forward(
        &self,
        input_ids: &Tensor,
        pixel_values: Option<Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
        position_ids: Vec<usize>,
        _model_specific_args: Box<dyn std::any::Any>, // pixel attention mask, or image sizes, or anything else
    ) -> candle_core::Result<Tensor> {
        self.forward_inputs(
            input_ids,
            pixel_values,
            Some(
                self.clip_vision_tower.num_patches_per_side()
                    * self.clip_vision_tower.num_patches_per_side(),
            ),
            seqlen_offsets,
            start_offsets_kernel,
            context_lens,
            position_ids,
        )
    }

    fn device(&self) -> &Device {
        &self.device
    }

    fn cache(&self) -> &crate::pipeline::Cache {
        self.llm.cache()
    }

    fn max_seq_len(&self) -> usize {
        self.config.text_config.max_length
    }

    fn has_conv2d(&self) -> bool {
        true
    }
}
