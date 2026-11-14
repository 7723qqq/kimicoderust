use std::sync::Arc;

use serde_json::Value;

use crate::_base::di::lifecycle::IDisposable;
use crate::app::config::config::IConfigService;
use crate::config::ImageConfig;

use super::image_compress::{
    set_configured_max_image_edge_px, set_configured_read_image_byte_budget,
};

pub struct ImageConfigBridge {
    subscription: Box<dyn IDisposable>,
}

impl ImageConfigBridge {
    pub fn new(config: Arc<dyn IConfigService>) -> Self {
        push(config.get("image"));
        let subscription = config.on_did_section_change(Box::new(|event| {
            if event.domain == "image" {
                push(event.value);
            }
        }));
        Self { subscription }
    }

    pub fn dispose(&self) {
        self.subscription.dispose();
    }
}

impl Drop for ImageConfigBridge {
    fn drop(&mut self) {
        self.subscription.dispose();
    }
}

fn push(value: Option<Value>) {
    let image = value.and_then(|value| serde_json::from_value::<ImageConfig>(value).ok());
    set_configured_max_image_edge_px(image.as_ref().and_then(|image| image.max_edge_px));
    set_configured_read_image_byte_budget(
        image
            .as_ref()
            .and_then(|image| image.read_byte_budget)
            .and_then(|value| usize::try_from(value).ok()),
    );
}
