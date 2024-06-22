use std::fmt::Debug;

use crate::utils::debug::DeviceRepr;
use candle_core::{Device, Result, Tensor};
use candle_nn::VarBuilder;
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Default, Deserialize, Clone)]
pub struct DeviceLayerMapMetadata {
    pub ordinal: usize,
    pub layers: usize,
}

#[derive(Debug, Default, Deserialize, Clone)]
/// Metadata to initialize the device mapper.
pub struct DeviceMapMetadata {
    device_layers: Option<Vec<DeviceLayerMapMetadata>>,
    host_layers: Option<usize>,
}

impl DeviceMapMetadata {
    #[deprecated(note = "This will be replaced by a more general API to handle multi-GPU.")]
    pub fn from_num_device_layers(device_layers: usize) -> Self {
        // TODO(EricLBuehler): Hardcoding is bad but we are creating the device on ord 0 anyway.
        Self {
            device_layers: Some(vec![DeviceLayerMapMetadata {
                ordinal: 0,
                layers: device_layers,
            }]),
            host_layers: None,
        }
    }
    // TODO(EricLBuehler): For version 0.2.0, replace `from_num_device_layers` with this.
    pub fn from_num_device_layers_multi_gpu(device_layers: Vec<DeviceLayerMapMetadata>) -> Self {
        Self {
            device_layers: Some(device_layers),
            host_layers: None,
        }
    }
    /// A device mapper to not map device.
    pub fn dummy() -> Self {
        Self {
            device_layers: None,
            host_layers: None,
        }
    }
    pub fn is_dummy(&self) -> bool {
        self.device_layers.is_none()
    }
    pub fn into_mapper(
        &self,
        model_layers: usize,
        device: &Device,
    ) -> Result<Box<dyn DeviceMapper + Send + Sync>> {
        // How many device layers
        // Clamp to max of model layers
        let n_device_layers = if let Some(layers) = &self.device_layers {
            layers
                .iter()
                .map(|metadata| metadata.layers)
                .sum::<usize>()
                .clamp(0, model_layers)
        } else {
            return Ok(Box::new(DummyDeviceMapper {
                nm_device: device.clone(),
            }));
        };
        // How many host (cpu) layers, defaulting to automatically filling the rest.
        // If n_device_layers > model_layers, n_host_layers = 0
        let n_host_layers = self
            .host_layers
            .unwrap_or(model_layers.saturating_sub(n_device_layers));
        if n_device_layers + n_host_layers != model_layers {
            candle_core::bail!("Expected the total number of GPU ({n_device_layers}) and host layers ({n_host_layers}) to sum to the number of model hidden layers ({model_layers})");
        }
        info!("Model has {model_layers} repeating layers.");

        // Handle multi-GPU mapping here
        let mut combined = Vec::with_capacity(model_layers);
        if self
            .device_layers
            .as_ref()
            .is_some_and(|layers| layers.len() == 1)
        {
            combined.extend(vec![device.clone(); n_device_layers]);
        } else {
            for DeviceLayerMapMetadata { ordinal, layers } in self.device_layers.as_ref().unwrap() {
                let dev = match device {
                    Device::Cpu => Device::Cpu,
                    Device::Cuda(_) => Device::cuda_if_available(*ordinal)?,
                    Device::Metal(_) => Device::new_metal(*ordinal)?,
                };
                combined.extend(vec![dev; *layers]);
            }
        }

        // Always put the CPU layers at the end so that we reduce dtoh and htod copies
        combined.extend(vec![Device::Cpu; n_host_layers]);

        // Sanity
        assert_eq!(combined.len(), model_layers);

        info!("Loading model according to the following repeating layer mappings:");
        for (i, dev) in combined.iter().enumerate() {
            info!("Layer {i}: {}", dev.device_pretty_repr());
        }

        Ok(Box::new(LayerDeviceMapper {
            mappings: combined,
            nm_device: device.clone(),
        }))
    }
}

pub trait DeviceMapper: Debug {
    /// Map during runtime
    fn map(&self, input: Tensor, layer: usize) -> Result<Tensor>;
    /// If ISQ layer, then do not change the device. *They will do it later in NormalModel::quantize*
    fn set_device<'a>(
        &self,
        layer: usize,
        varbuilder: VarBuilder<'a>,
        loading_isq: bool,
    ) -> VarBuilder<'a>;
    /// If ISQ layer, then do not change the device (return None). *They will do it later in NormalModel::quantize*
    fn device_for(&self, layer: usize, loading_isq: bool) -> Option<&Device>;
    /// Set non mapped layer device. This is for ISQ + device mapping support
    /// If ISQ layer, then do not change the device. *They will do it later in NormalModel::quantize*
    fn set_nm_device<'a>(&self, varbuilder: VarBuilder<'a>, loading_isq: bool) -> VarBuilder<'a>;
}

#[derive(Debug)]
/// A device mapper which does device mapping per hidden layer.
pub struct LayerDeviceMapper {
    mappings: Vec<Device>,
    nm_device: Device,
}

impl DeviceMapper for LayerDeviceMapper {
    fn map(&self, input: Tensor, layer: usize) -> Result<Tensor> {
        input.to_device(&self.mappings[layer])
    }
    fn set_device<'a>(
        &self,
        layer: usize,
        varbuilder: VarBuilder<'a>,
        loading_isq: bool,
    ) -> VarBuilder<'a> {
        if loading_isq {
            return varbuilder;
        }
        varbuilder.set_device(self.mappings[layer].clone())
    }
    fn device_for(&self, layer: usize, loading_isq: bool) -> Option<&Device> {
        if loading_isq {
            return Some(&self.nm_device);
        }
        self.mappings.get(layer)
    }
    fn set_nm_device<'a>(&self, varbuilder: VarBuilder<'a>, loading_isq: bool) -> VarBuilder<'a> {
        if loading_isq {
            varbuilder
        } else {
            varbuilder.set_device(self.nm_device.clone())
        }
    }
}

#[derive(Debug)]
pub struct DummyDeviceMapper {
    nm_device: Device,
}

impl DeviceMapper for DummyDeviceMapper {
    fn map(&self, input: Tensor, _: usize) -> Result<Tensor> {
        Ok(input)
    }
    fn set_device<'a>(
        &self,
        _: usize,
        varbuilder: VarBuilder<'a>,
        loading_isq: bool,
    ) -> VarBuilder<'a> {
        if loading_isq {
            varbuilder.set_device(Device::Cpu)
        } else {
            varbuilder.set_device(self.nm_device.clone())
        }
    }
    fn device_for(&self, _: usize, loading_isq: bool) -> Option<&Device> {
        if loading_isq {
            return Some(&self.nm_device);
        }
        None
    }
    fn set_nm_device<'a>(&self, varbuilder: VarBuilder<'a>, loading_isq: bool) -> VarBuilder<'a> {
        if loading_isq {
            varbuilder.set_device(Device::Cpu)
        } else {
            varbuilder.set_device(self.nm_device.clone())
        }
    }
}
