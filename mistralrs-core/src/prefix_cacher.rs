use std::{
    collections::{hash_map, HashMap},
    path::Path,
    sync::{Arc, Mutex},
};

use candle_core::{Device, Result, Tensor};
use radix_trie::{Trie, TrieCommon, TrieKey};

use crate::{get_mut_arcmutex, pipeline::LayerCaches, sequence::Sequence};

#[derive(PartialEq, Eq, Clone)]
struct Tokens(Vec<u32>);

impl TrieKey for Tokens {
    fn encode_bytes(&self) -> Vec<u8> {
        self.0
            .iter()
            .flat_map(|x| bytemuck::bytes_of(x).to_vec())
            .collect::<Vec<u8>>()
    }
}

impl From<Vec<u32>> for Tokens {
    fn from(value: Vec<u32>) -> Self {
        Self(value)
    }
}

type EvictionCacheGroup = (Arc<Mutex<LayerCaches>>, Option<Arc<Mutex<LayerCaches>>>);

pub struct PrefixCacheManager {
    caches: Trie<Tokens, Arc<Mutex<LayerCaches>>>,
    xlora_caches: Option<Trie<Tokens, Arc<Mutex<LayerCaches>>>>,
    device: Device,
    pub n_on_device: usize,
    no_prefix_cache: bool,
    eviction_cache_ptrs: Vec<EvictionCacheGroup>,
}

#[derive(Clone)]
pub struct MatchingCache {
    pub normal: LayerCaches,
    pub xlora: Option<LayerCaches>,
    pub toks: Vec<u32>,
}

impl PrefixCacheManager {
    pub fn new(device: Device, n_on_device: usize, is_xlora: bool, no_prefix_cache: bool) -> Self {
        PrefixCacheManager {
            caches: Trie::new(),
            xlora_caches: if is_xlora { Some(Trie::new()) } else { None },
            device,
            n_on_device,
            no_prefix_cache,
            eviction_cache_ptrs: Vec::new(),
        }
    }

    fn save_layer_caches<'a>(
        file: impl AsRef<Path>,
        caches: impl Iterator<Item = (&'a Tokens, &'a Arc<Mutex<LayerCaches>>)>,
    ) -> Result<()> {
        let mut tensor_map = HashMap::new();
        for (toks, kvs) in caches {
            let mut ks = Vec::new();
            let mut vs = Vec::new();
            // Really, we only have Some variants. This is just for the type system.
            let kvs = get_mut_arcmutex!(kvs)
                .iter()
                .filter_map(|x| x.clone())
                .collect::<Vec<_>>();
            for (k, v) in kvs {
                ks.push(k.to_device(&Device::Cpu)?);
                vs.push(v.to_device(&Device::Cpu)?);
            }

            let k = Tensor::stack(&ks, 0)?;
            let v = Tensor::stack(&vs, 0)?;
            // NOTE(EricLBuehler): changing this format is a *breaking change*
            let repr = toks
                .0
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(";");
            // NOTE(EricLBuehler): changing this format is a *breaking change*
            tensor_map.insert(format!("K:{repr}"), k);
            // NOTE(EricLBuehler): changing this format is a *breaking change*
            tensor_map.insert(format!("V:{repr}"), v);
        }
        candle_core::safetensors::save(&tensor_map, file)
    }

    /// Save the prefix cache to the specified file (including X-LoRA if applicable)
    /// Example of file: `prefix_cache.safetensors`
    /// This function will save the content at the paths:
    /// - `prefix_cache.safetensors.{model_id}`
    /// - If X-LoRA: `prefix_cache.safetensors.{model_id}.xlora`
    pub fn save_to_file(&self, file: impl AsRef<Path>, model_id: String) -> Result<()> {
        let file = file.as_ref().to_string_lossy().to_string();
        // NOTE(EricLBuehler): changing this format is a *breaking change*
        let formatted_file = format!("{file}.{model_id}");
        Self::save_layer_caches(formatted_file.clone(), self.caches.iter())?;

        if self.xlora_caches.is_some() {
            // NOTE(EricLBuehler): changing this format is a *breaking change*
            let formatted_file_xlora = format!("{formatted_file}.xlora");
            Self::save_layer_caches(
                formatted_file_xlora,
                self.xlora_caches.as_ref().unwrap().iter(),
            )?;
        }

        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn load_layer_caches(
        file: impl AsRef<Path>,
        device: Device,
        n_on_device: usize,
    ) -> Result<(
        Trie<Tokens, Arc<Mutex<LayerCaches>>>,
        Vec<EvictionCacheGroup>,
    )> {
        let map = candle_core::safetensors::load(file, &Device::Cpu)?;
        // Contains (k,v) tensors which are stacked along dim0 on the layer length.
        let mut shards = HashMap::new();
        for (name, tensor) in map {
            // NOTE(EricLBuehler): changing this format is a *breaking change*
            let parts = name.splitn(2, ':').collect::<Vec<_>>();
            let ty = parts[0];
            let toks = parts[1]
                .split(':')
                .map(|x| x.parse::<u32>().expect("Failed to parse token"))
                .collect::<Vec<_>>();
            if shards
                .get(&toks)
                .is_some_and(|x: &(Option<Tensor>, Option<Tensor>)| x.0.is_some())
            {
                // NOTE(EricLBuehler): changing this format is a *breaking change*
                // Add V
                assert_eq!(ty, "V");
                shards.get_mut(&toks).unwrap().1 = Some(tensor);
            } else if shards
                .get(&toks)
                .is_some_and(|x: &(Option<Tensor>, Option<Tensor>)| x.0.is_some())
            {
                // NOTE(EricLBuehler): changing this format is a *breaking change*
                // Add K
                assert_eq!(ty, "K");
                shards.get_mut(&toks).unwrap().0 = Some(tensor);
            } else if let hash_map::Entry::Vacant(e) = shards.entry(toks) {
                // NOTE(EricLBuehler): changing this format is a *breaking change*
                if ty == "K" {
                    e.insert((Some(tensor), None));
                } else if ty == "V" {
                    e.insert((None, Some(tensor)));
                } else {
                    panic!("Got unexpected KV shard type {ty}");
                }
            } else {
                unreachable!()
            }
        }
        let mut trie = Trie::new();
        let mut eviction_cache_ptrs = Vec::new();
        let mut n_prefixes = 0;
        for (toks, (k, v)) in shards {
            n_prefixes += 1;
            let dev = if n_prefixes <= n_on_device {
                device.clone()
            } else {
                Device::Cpu
            };
            let k = k.unwrap();
            let v = v.unwrap();
            let ks = k
                .chunk(k.dim(0)?, 0)?
                .iter()
                .map(|x| x.squeeze(0))
                .collect::<Result<Vec<_>>>()?;
            let vs = v
                .chunk(k.dim(0)?, 0)?
                .iter()
                .map(|x| x.squeeze(0))
                .collect::<Result<Vec<_>>>()?;
            let mut layer_caches = Vec::new();
            for (k, v) in ks.into_iter().zip(vs) {
                layer_caches.push(Some((k.to_device(&dev)?, v.to_device(&dev)?)));
            }
            let layer_caches = Arc::new(Mutex::new(layer_caches));
            trie.insert(Tokens(toks), layer_caches.clone());
            eviction_cache_ptrs.push((layer_caches, None));
        }
        Ok((trie, eviction_cache_ptrs))
    }

    /// Create a prefix cache manager from prefixes in a .safetensor file, extending the current state.
    ///
    /// The file must have been
    /// - Saved with `PrefixCacheManager::save_to_file`.
    /// - Saved with a version of `mistral.rs` to which the current version is backwards compatible
    ///
    /// For example, if you save pass `save_to_file` the file `prefix_cache.safetensors`, you should also pass
    /// this function `prefix_cache.safetensors.`
    /// This function expects content at the paths:
    /// - `prefix_cache.safetensors.{model_id}`
    /// - If X-LoRA: `prefix_cache.safetensors.{model_id}.xlora`
    pub fn load_from_file(
        mut self,
        file: impl AsRef<Path>,
        device: Device,
        n_on_device: usize,
        model_id: String,
    ) -> Result<Self> {
        // NOTE(EricLBuehler): changing this format is a *breaking change*
        let file = file.as_ref().to_string_lossy().to_string();
        let formatted_file = format!("{file}.{model_id}");
        let (trie, ptrs) =
            Self::load_layer_caches(formatted_file.clone(), device.clone(), n_on_device)?;
        // NOTE: Don't like the copy...
        for (k, v) in trie.iter() {
            self.caches.insert(k.clone(), v.clone());
        }
        self.eviction_cache_ptrs.extend(ptrs);

        if self.xlora_caches.is_some() {
            // NOTE(EricLBuehler): changing this format is a *breaking change*
            let formatted_file_xlora = format!("{formatted_file}.xlora");
            let (trie, ptrs) = Self::load_layer_caches(formatted_file_xlora, device, n_on_device)?;

            // NOTE: Don't like the copy...
            for (k, v) in trie.iter() {
                self.xlora_caches
                    .as_mut()
                    .unwrap()
                    .insert(k.clone(), v.clone());
            }
            self.eviction_cache_ptrs.extend(ptrs);
        }
        Ok(self)
    }

    /// This always keeps the cache on the device. If later on, a new seq cannot be allocated due to memory shortage,
    /// some caches will be evicted.
    pub fn add_sequence(&mut self, seq: &mut Sequence) {
        if self.no_prefix_cache {
            return;
        }
        let cache = Arc::new(Mutex::new(seq.cache().clone()));
        self.caches
            .insert(seq.get_toks().to_vec().into(), cache.clone());
        if seq.is_xlora() {
            let xlora_cache = Arc::new(Mutex::new(seq.xlora_cache().clone()));
            self.xlora_caches
                .as_mut()
                .unwrap()
                .insert(seq.get_toks().to_vec().into(), xlora_cache.clone());
            self.eviction_cache_ptrs.push((cache, Some(xlora_cache)));
        } else {
            self.eviction_cache_ptrs.push((cache, None));
        }
    }

    fn cache_to<'a>(
        cache: impl Iterator<Item = &'a mut Option<(Tensor, Tensor)>>,
        device: &Device,
    ) -> Result<()> {
        for layer in cache {
            if let Some((ref q, ref k)) = layer {
                *layer = Some((q.to_device(device)?, k.to_device(device)?));
            }
        }
        Ok(())
    }

    /// Evict the caches to CPU. This will evict the first k seqs such that the number of sequences on device after the copy is
    /// the maximum allowed. Returns the number of evicted sequences.
    pub fn evict_to_cpu(&mut self) -> Result<usize> {
        if self.no_prefix_cache {
            return Ok(0);
        }
        let mut n_on_device = 0;
        for (cache, _) in &self.eviction_cache_ptrs {
            if get_mut_arcmutex!(cache.as_ref())[0].is_none() {
                // TODO: add support for normal cache
                continue;
            }
            if !matches!(
                get_mut_arcmutex!(cache.as_ref())[0]
                    .as_ref()
                    .unwrap()
                    .0
                    .device(),
                Device::Cpu
            ) {
                n_on_device += 1;
            }
        }
        let mut n_evicted = 0;
        // Intentionally evict the first ones first, as they are the oldest
        for (cache, xlora_cache) in &self.eviction_cache_ptrs {
            if n_on_device - n_evicted == self.n_on_device {
                break;
            }
            if get_mut_arcmutex!(cache.as_ref())[0].is_none() {
                // TODO: add support for normal cache
                continue;
            }
            if !matches!(
                get_mut_arcmutex!(cache.as_ref())[0]
                    .as_ref()
                    .unwrap()
                    .0
                    .device(),
                Device::Cpu
            ) {
                let mut cache = get_mut_arcmutex!(cache);
                let mut xlora_cache = xlora_cache.as_ref().map(|c| get_mut_arcmutex!(c));

                Self::cache_to(cache.iter_mut(), &Device::Cpu)?;
                if let Some(ref mut xlora_cache) = xlora_cache {
                    Self::cache_to(xlora_cache.iter_mut(), &Device::Cpu)?;
                }
                n_evicted += 1;
            }
        }
        Ok(self.caches.len().saturating_sub(self.n_on_device))
    }

    /// Evict all the caches to CPU.
    pub fn evict_all_to_cpu(&mut self) -> Result<usize> {
        if self.no_prefix_cache {
            return Ok(0);
        }
        // Intentionally evict the first ones first, as they are the oldest
        for (cache, xlora_cache) in &self.eviction_cache_ptrs {
            if get_mut_arcmutex!(cache.as_ref())[0].is_none() {
                // TODO: add support for normal cache
                continue;
            }
            if !matches!(
                get_mut_arcmutex!(cache.as_ref())[0]
                    .as_ref()
                    .unwrap()
                    .0
                    .device(),
                Device::Cpu
            ) {
                let mut cache = get_mut_arcmutex!(cache);
                let mut xlora_cache = xlora_cache.as_ref().map(|c| get_mut_arcmutex!(c));

                Self::cache_to(cache.iter_mut(), &Device::Cpu)?;
                if let Some(ref mut xlora_cache) = xlora_cache {
                    Self::cache_to(xlora_cache.iter_mut(), &Device::Cpu)?;
                }
            }
        }
        Ok(self.caches.len())
    }

    /// Search for a matching cache given some toks
    pub fn search_for_matching_cache(&mut self, toks: &[u32]) -> Result<Option<MatchingCache>> {
        if self.no_prefix_cache || toks.is_empty() {
            return Ok(None);
        }

        let toks = Tokens(toks.to_vec());
        if let Some(cache) = self.caches.get(&toks) {
            Self::cache_to(get_mut_arcmutex!(cache.as_ref()).iter_mut(), &self.device)?;
            let cache = get_mut_arcmutex!(cache.as_ref()).clone();
            let xlora_cache = if let Some(ref xlora_caches) = self.xlora_caches {
                let mut xlora_cache = get_mut_arcmutex!(xlora_caches.get(&toks).unwrap().as_ref());
                Self::cache_to(xlora_cache.iter_mut(), &self.device)?;
                Some(xlora_cache.clone())
            } else {
                None
            };
            let ancestor = &self
                .caches
                .get_ancestor(&toks)
                .expect("No ancestor.")
                .key()
                .expect("Cannot get the key.")
                .0;
            // Know ancestor.len() < toks.len(), and toks[0..ancestor.len()] == toks
            Ok(Some(MatchingCache {
                normal: cache,
                xlora: xlora_cache,
                toks: toks.0[ancestor.len()..].to_vec(),
            }))
        } else {
            Ok(None)
        }
    }
}
