use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::Result;
use hf_hub::{
    api::sync::{ApiBuilder, ApiRepo},
    Repo, RepoType,
};
use serde_json::Value;
use tracing::{info, warn};

use crate::{
    api_dir_list, api_get_file, lora::LoraConfig, utils::tokens::get_token,
    xlora_models::XLoraConfig, ModelPaths, Ordering, TokenSource,
};

use super::chat_template::ChatTemplate;

pub(crate) struct XLoraPaths {
    pub adapter_configs: Option<Vec<((String, String), LoraConfig)>>,
    pub adapter_safetensors: Option<Vec<(String, PathBuf)>>,
    pub classifier_path: Option<PathBuf>,
    pub xlora_order: Option<Ordering>,
    pub xlora_config: Option<XLoraConfig>,
    pub lora_preload_adapter_info: Option<HashMap<String, (PathBuf, LoraConfig)>>,
}

pub fn get_xlora_paths(
    base_model_id: String,
    xlora_model_id: &Option<String>,
    token_source: &TokenSource,
    revision: String,
    xlora_order: &Option<Ordering>,
) -> Result<XLoraPaths> {
    Ok(if let Some(ref xlora_id) = xlora_model_id {
        let api = ApiBuilder::new()
            .with_progress(true)
            .with_token(get_token(token_source)?)
            .build()?;
        let api = api.repo(Repo::with_revision(
            xlora_id.clone(),
            RepoType::Model,
            revision,
        ));
        let model_id = Path::new(&xlora_id);

        // Get the path for the xlora classifier
        let xlora_classifier = &api_dir_list!(api, model_id)
            .filter(|x| x.contains("xlora_classifier.safetensors"))
            .collect::<Vec<_>>();
        if xlora_classifier.len() > 1 {
            warn!("Detected multiple X-LoRA classifiers: {xlora_classifier:?}");
            warn!("Selected classifier: `{}`", &xlora_classifier[0]);
        }
        let xlora_classifier = xlora_classifier.first();

        let classifier_path =
            xlora_classifier.map(|xlora_classifier| api_get_file!(api, xlora_classifier, model_id));

        // Get the path for the xlora config by checking all for valid versions.
        // NOTE(EricLBuehler): Remove this functionality because all configs should be deserializable
        let xlora_configs = &api_dir_list!(api, model_id)
            .filter(|x| x.contains("xlora_config.json"))
            .collect::<Vec<_>>();
        if xlora_configs.len() > 1 {
            warn!("Detected multiple X-LoRA configs: {xlora_configs:?}");
        }

        let mut xlora_config: Option<XLoraConfig> = None;
        let mut last_err: Option<serde_json::Error> = None;
        for (i, config_path) in xlora_configs.iter().enumerate() {
            if xlora_configs.len() != 1 {
                warn!("Selecting config: `{}`", config_path);
            }
            let config_path = api_get_file!(api, config_path, model_id);
            let conf = fs::read_to_string(config_path)?;
            let deser: Result<XLoraConfig, serde_json::Error> = serde_json::from_str(&conf);
            match deser {
                Ok(conf) => {
                    xlora_config = Some(conf);
                    break;
                }
                Err(e) => {
                    if i != xlora_configs.len() - 1 {
                        warn!("Config is broken with error `{e}`");
                    }
                    last_err = Some(e);
                }
            }
        }
        let xlora_config = xlora_config.map(Some).unwrap_or_else(|| {
            if let Some(last_err) = last_err {
                panic!(
                    "Unable to derserialize any configs. Last error: {}",
                    last_err
                )
            } else {
                None
            }
        });

        // If there are adapters in the ordering file, get their names and remote paths
        let adapter_files = api_dir_list!(api, model_id)
            .filter_map(|name| {
                if let Some(ref adapters) = xlora_order.as_ref().unwrap().adapters {
                    for adapter_name in adapters {
                        if name.contains(adapter_name) {
                            return Some((name, adapter_name.clone()));
                        }
                    }
                }
                None
            })
            .collect::<Vec<_>>();
        if adapter_files.is_empty() && xlora_order.as_ref().unwrap().adapters.is_some() {
            anyhow::bail!("Adapter files are empty. Perhaps the ordering file adapters does not match the actual adapters?")
        }

        // Get the local paths for each adapter
        let mut adapters_paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for (file, name) in adapter_files {
            if let Some(paths) = adapters_paths.get_mut(&name) {
                paths.push(api_get_file!(api, &file, model_id));
            } else {
                adapters_paths.insert(name, vec![api_get_file!(api, &file, model_id)]);
            }
        }

        // Sort local paths for the adapter configs and safetensors files
        let mut adapters_configs = Vec::new();
        let mut adapters_safetensors = Vec::new();
        if let Some(ref adapters) = xlora_order.as_ref().unwrap().adapters {
            for (i, name) in adapters.iter().enumerate() {
                let paths = adapters_paths
                    .get(name)
                    .unwrap_or_else(|| panic!("Adapter {name} not found."));
                for path in paths {
                    if path.extension().unwrap() == "safetensors" {
                        adapters_safetensors.push((name.clone(), path.to_owned()));
                    } else {
                        let conf = fs::read_to_string(path)?;
                        let lora_config: LoraConfig = serde_json::from_str(&conf)?;
                        adapters_configs.push((((i + 1).to_string(), name.clone()), lora_config));
                    }
                }
            }
        }

        // Make sure they all match
        if xlora_order.as_ref().is_some_and(|order| {
            &order.base_model_id
                != xlora_config
                    .as_ref()
                    .map(|cfg| &cfg.base_model_id)
                    .unwrap_or(&base_model_id)
        }) || xlora_config
            .as_ref()
            .map(|cfg| &cfg.base_model_id)
            .unwrap_or(&base_model_id)
            != &base_model_id
        {
            anyhow::bail!(
                "Adapter ordering file, adapter model config, and base model ID do not match: {}, {}, and {} respectively.",
                xlora_order.as_ref().unwrap().base_model_id,
                xlora_config.map(|cfg| cfg.base_model_id).unwrap_or(base_model_id.clone()),
                base_model_id
            );
        }

        let lora_preload_adapter_info = if let Some(xlora_order) = xlora_order {
            // If preload adapters are specified, get their metadata like above
            if let Some(preload_adapters) = &xlora_order.preload_adapters {
                let mut output = HashMap::new();
                for adapter in preload_adapters {
                    // Get the names and remote paths of the files associated with this adapter
                    let adapter_files = api_dir_list!(api, &adapter.adapter_model_id)
                        .filter_map(|f| {
                            if f.contains(&adapter.name) {
                                Some((f, adapter.name.clone()))
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>();
                    if adapter_files.is_empty() {
                        anyhow::bail!("Adapter files are empty. Perhaps the ordering file adapters does not match the actual adapters?")
                    }
                    // Get local paths for this adapter
                    let mut adapters_paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
                    for (file, name) in adapter_files {
                        if let Some(paths) = adapters_paths.get_mut(&name) {
                            paths.push(api_get_file!(api, &file, model_id));
                        } else {
                            adapters_paths.insert(name, vec![api_get_file!(api, &file, model_id)]);
                        }
                    }

                    let mut config = None;
                    let mut safetensor = None;

                    // Sort local paths for the adapter configs and safetensors files
                    let paths = adapters_paths
                        .get(&adapter.name)
                        .unwrap_or_else(|| panic!("Adapter {} not found.", adapter.name));
                    for path in paths {
                        if path.extension().unwrap() == "safetensors" {
                            safetensor = Some(path.to_owned());
                        } else {
                            let conf = fs::read_to_string(path)?;
                            let lora_config: LoraConfig = serde_json::from_str(&conf)?;
                            config = Some(lora_config);
                        }
                    }

                    let (config, safetensor) = (config.unwrap(), safetensor.unwrap());
                    output.insert(adapter.name.clone(), (safetensor, config));
                }
                Some(output)
            } else {
                None
            }
        } else {
            None
        };

        XLoraPaths {
            adapter_configs: Some(adapters_configs),
            adapter_safetensors: Some(adapters_safetensors),
            classifier_path,
            xlora_order: xlora_order.clone(),
            xlora_config,
            lora_preload_adapter_info,
        }
    } else {
        XLoraPaths {
            adapter_configs: None,
            adapter_safetensors: None,
            classifier_path: None,
            xlora_order: None,
            xlora_config: None,
            lora_preload_adapter_info: None,
        }
    })
}

pub fn get_model_paths(
    revision: String,
    token_source: &TokenSource,
    quantized_model_id: &Option<String>,
    quantized_filename: &Option<String>,
    api: &ApiRepo,
    model_id: &Path,
) -> Result<Vec<PathBuf>> {
    match &quantized_filename {
        Some(name) => match quantized_model_id.as_ref().unwrap().as_str() {
            "" => Ok(vec![PathBuf::from_str(name).unwrap()]),
            id => {
                let qapi = ApiBuilder::new()
                    .with_progress(true)
                    .with_token(get_token(token_source)?)
                    .build()?;
                let qapi = qapi.repo(Repo::with_revision(
                    id.to_string(),
                    RepoType::Model,
                    revision.clone(),
                ));
                let model_id = Path::new(&id);
                Ok(vec![api_get_file!(qapi, name, model_id)])
            }
        },
        None => {
            let mut filenames = vec![];
            for rfilename in api_dir_list!(api, model_id).filter(|x| x.ends_with(".safetensors")) {
                filenames.push(api_get_file!(api, &rfilename, model_id));
            }
            Ok(filenames)
        }
    }
}

/// Find and parse the appropriate [`ChatTemplate`], and ensure is has a valid [`ChatTemplate.chat_template`].
/// If the the provided `tokenizer_config.json` from [`ModelPaths.get_template_filename`] does not
/// have a `chat_template`, use the provided one.
#[allow(clippy::borrowed_box)]
pub(crate) fn get_chat_template(
    paths: &Box<dyn ModelPaths>,
    chat_template: &Option<String>,
) -> ChatTemplate {
    let template_filename = if paths.get_template_filename().to_string_lossy().is_empty() {
        PathBuf::from(
            chat_template
                .as_ref()
                .expect("A tokenizer config or chat template file path must be specified."),
        )
    } else {
        paths.get_template_filename().clone()
    };
    if template_filename
        .extension()
        .expect("Template filename must be a file")
        .to_string_lossy()
        != "json"
    {
        panic!("Template filename {template_filename:?} must end with `.json`.");
    }
    #[derive(Debug, serde::Deserialize)]
    struct SpecifiedTemplate {
        chat_template: String,
        bos_token: Option<String>,
        eos_token: Option<String>,
    }

    info!("`tokenizer_config.json` does not contain a chat template, attempting to use specified JINJA chat template.");
    let mut deser: HashMap<String, Value> =
        serde_json::from_str(&fs::read_to_string(&template_filename).unwrap()).unwrap();

    match chat_template.clone() {
        Some(t) => {
            if t.ends_with(".json") {
                info!("Loading specified loading chat template file at `{t}`.");
                let templ: SpecifiedTemplate =
                    serde_json::from_str(&fs::read_to_string(t.clone()).unwrap()).unwrap();
                deser.insert(
                    "chat_template".to_string(),
                    Value::String(templ.chat_template),
                );
                if templ.bos_token.is_some() {
                    deser.insert(
                        "bos_token".to_string(),
                        Value::String(templ.bos_token.unwrap()),
                    );
                }
                if templ.eos_token.is_some() {
                    deser.insert(
                        "eos_token".to_string(),
                        Value::String(templ.eos_token.unwrap()),
                    );
                }
                info!("Loaded chat template file.");
            } else {
                deser.insert("chat_template".to_string(), Value::String(t));
                info!("Loaded specified literal chat template.");
            }
        }
        None => {
            info!("No specified chat template. No chat template will be used. Only prompts will be accepted, not messages.");
            deser.insert("chat_template".to_string(), Value::Null);
        }
    };
    let ser = serde_json::to_string_pretty(&deser)
        .expect("Serialization of modified chat template failed.");
    serde_json::from_str(&ser).unwrap()
}
