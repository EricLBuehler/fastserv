use anyhow::Context;
use candle_core::{Device, Result};
use mistralrs_core::*;
use std::{num::NonZeroUsize, path::PathBuf, sync::Arc};
use tokio::sync::mpsc::channel;

use crate::RequestLike;

/// Gets the best device, cpu, cuda if compiled with CUDA
fn best_device(force_cpu: bool) -> Result<Device> {
    if force_cpu {
        return Ok(Device::Cpu);
    }
    #[cfg(not(feature = "metal"))]
    {
        Device::cuda_if_available(0)
    }
    #[cfg(feature = "metal")]
    {
        Device::new_metal(0)
    }
}

pub struct TextModel {
    runner: Arc<MistralRs>,
}

pub struct TextModelBuilder {
    // Loading model
    model_id: String,
    token_source: TokenSource,
    hf_revision: Option<String>,
    write_uqff: Option<PathBuf>,
    from_uqff: Option<PathBuf>,
    chat_template: Option<String>,
    tokenizer_json: Option<String>,

    // Model running
    use_flash_attn: bool,
    prompt_batchsize: Option<NonZeroUsize>,
    topology: Option<Topology>,
    organization: IsqOrganization,
    loader_type: Option<NormalLoaderType>,
    dtype: ModelDType,
    force_cpu: bool,
    isq: Option<IsqType>,

    // Other things
    paged_attn_cfg: Option<PagedAttentionConfig>,
    max_num_seqs: usize,
    no_kv_cache: bool,
    with_logging: bool,
    prefix_cache_n: Option<usize>,
}

pub struct PagedAttentionMetaBuilder {
    block_size: Option<usize>,
    mem_cpu: usize,
    mem_gpu: MemoryGpuConfig,
}

impl Default for PagedAttentionMetaBuilder {
    fn default() -> Self {
        Self {
            block_size: None,
            mem_cpu: 64,
            mem_gpu: MemoryGpuConfig::Utilization(0.9),
        }
    }
}

impl PagedAttentionMetaBuilder {
    pub fn with_block_size(mut self, block_size: usize) -> Self {
        self.block_size = Some(block_size);
        self
    }

    pub fn with_gpu_memory(mut self, mem_gpu: MemoryGpuConfig) -> Self {
        self.mem_gpu = mem_gpu;
        self
    }

    pub fn build(self) -> anyhow::Result<PagedAttentionConfig> {
        PagedAttentionConfig::new(self.block_size, self.mem_cpu, self.mem_gpu)
    }
}

impl TextModelBuilder {
    /// A few defaults are applied here:
    /// - MoQE ISQ organization
    /// - Token source is from the cache (.cache/huggingface/token)
    /// - Maximum number of sequences running is 32
    /// - Number of sequences to hold in prefix cache is 16.
    pub fn new(model_id: String) -> Self {
        Self {
            model_id,
            use_flash_attn: cfg!(feature = "flash-attn"),
            prompt_batchsize: None,
            topology: None,
            organization: IsqOrganization::Default,
            write_uqff: None,
            from_uqff: None,
            chat_template: None,
            tokenizer_json: None,
            loader_type: None,
            dtype: ModelDType::Auto,
            force_cpu: false,
            token_source: TokenSource::CacheToken,
            hf_revision: None,
            isq: None,
            paged_attn_cfg: None,
            max_num_seqs: 32,
            no_kv_cache: false,
            prefix_cache_n: Some(16),
            with_logging: false,
        }
    }

    pub fn with_prompt_batchsize(mut self, prompt_batchsize: NonZeroUsize) -> Self {
        self.prompt_batchsize = Some(prompt_batchsize);
        self
    }

    pub fn with_topology(mut self, topology: Topology) -> Self {
        self.topology = Some(topology);
        self
    }

    pub fn with_mixture_qexperts_isq(mut self) -> Self {
        self.organization = IsqOrganization::MoeExpertsOnly;
        self
    }

    pub fn with_chat_template(mut self, chat_template: String) -> Self {
        self.chat_template = Some(chat_template);
        self
    }

    pub fn with_tokenizer_json(mut self, tokenizer_json: String) -> Self {
        self.tokenizer_json = Some(tokenizer_json);
        self
    }

    pub fn with_loader_type(mut self, loader_type: NormalLoaderType) -> Self {
        self.loader_type = Some(loader_type);
        self
    }

    pub fn with_dtype(mut self, dtype: ModelDType) -> Self {
        self.dtype = dtype;
        self
    }

    pub fn with_force_cpu(mut self) -> Self {
        self.force_cpu = true;
        self
    }

    pub fn with_token_source(mut self, token_source: TokenSource) -> Self {
        self.token_source = token_source;
        self
    }

    pub fn with_hf_revision(mut self, revision: String) -> Self {
        self.hf_revision = Some(revision);
        self
    }

    pub fn with_isq(mut self, isq: IsqType) -> Self {
        self.isq = Some(isq);
        self
    }

    pub fn with_paged_attn(mut self, paged_attn_cfg: PagedAttentionConfig) -> Self {
        self.paged_attn_cfg = Some(paged_attn_cfg);
        self
    }

    pub fn with_max_num_seqs(mut self, max_num_seqs: usize) -> Self {
        self.max_num_seqs = max_num_seqs;
        self
    }

    pub fn with_no_kv_cache(mut self) -> Self {
        self.no_kv_cache = true;
        self
    }

    pub fn with_prefix_cache_n(mut self, n_seqs: Option<usize>) -> Self {
        self.prefix_cache_n = n_seqs;
        self
    }

    pub fn with_logging(mut self) -> Self {
        self.with_logging = true;
        self
    }

    pub async fn build(self) -> anyhow::Result<TextModel> {
        let config = NormalSpecificConfig {
            use_flash_attn: self.use_flash_attn,
            prompt_batchsize: self.prompt_batchsize,
            topology: self.topology,
            organization: self.organization,
            write_uqff: self.write_uqff,
            from_uqff: self.from_uqff,
        };

        if self.with_logging {
            initialize_logging();
        }

        let loader = NormalLoaderBuilder::new(
            config,
            self.chat_template,
            self.tokenizer_json,
            Some(self.model_id),
        )
        .with_no_kv_cache(self.no_kv_cache)
        .build(self.loader_type)?;

        // Load, into a Pipeline
        let pipeline = loader.load_model_from_hf(
            self.hf_revision,
            self.token_source,
            &self.dtype,
            &best_device(self.force_cpu)?,
            !self.with_logging,
            DeviceMapMetadata::dummy(),
            self.isq,
            self.paged_attn_cfg,
        )?;

        let scheduler_method = match self.paged_attn_cfg {
            Some(_) => {
                let config = pipeline
                    .lock()
                    .await
                    .get_metadata()
                    .cache_config
                    .as_ref()
                    .unwrap()
                    .clone();

                SchedulerConfig::PagedAttentionMeta {
                    max_num_seqs: self.max_num_seqs,
                    config,
                }
            }
            None => SchedulerConfig::DefaultScheduler {
                method: DefaultSchedulerMethod::Fixed(self.max_num_seqs.try_into()?),
            },
        };

        let mut runner = MistralRsBuilder::new(pipeline, scheduler_method)
            .with_no_kv_cache(self.no_kv_cache)
            .with_gemm_full_precision_f16(true)
            .with_no_prefix_cache(self.prefix_cache_n.is_none());

        if let Some(n) = self.prefix_cache_n {
            runner = runner.with_prefix_cache_n(n)
        }

        Ok(TextModel::new(runner.build()))
    }
}

impl TextModel {
    pub fn new(runner: Arc<MistralRs>) -> Self {
        Self { runner }
    }

    /// See [`TextModelBuilder::new`] for the defaults which are applied.
    pub fn builder(model_id: String) -> TextModelBuilder {
        TextModelBuilder::new(model_id)
    }

    pub async fn send_chat_request<R: RequestLike>(
        &self,
        mut request: R,
    ) -> anyhow::Result<ChatCompletionResponse> {
        let (tx, mut rx) = channel(1);

        let (tools, tool_choice) = if let Some((a, b)) = request.take_tools() {
            (Some(a), Some(b))
        } else {
            (None, None)
        };
        let request = Request::Normal(NormalRequest {
            messages: RequestMessage::Chat(request.take_messages()),
            sampling_params: SamplingParams::default(),
            response: tx,
            return_logprobs: request.return_logprobs(),
            is_streaming: false,
            id: 0,
            constraint: request.take_constraint(),
            suffix: None,
            adapters: request.take_adapters(),
            tools,
            tool_choice,
            logits_processors: request.take_logits_processors(),
        });

        self.runner.get_sender()?.send(request).await?;

        let ResponseOk::Done(response) = rx
            .recv()
            .await
            .context("Channel was erroneously closed!")?
            .as_result()?
        else {
            anyhow::bail!("Got unexpected response type.")
        };

        Ok(response)
    }
}
