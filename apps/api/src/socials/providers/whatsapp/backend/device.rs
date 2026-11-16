use super::{Backend, DEVICE, encode};
use crate::socials::providers::whatsapp::vault::{Op, Row};
use async_trait::async_trait;
use std::sync::Arc;
use whatsapp_rust::wacore::store::{Device, error::Result, traits::DeviceStore};

#[async_trait]
impl DeviceStore for Backend {
    async fn save(&self, device: &Device) -> Result<()> {
        self.put_json(DEVICE, DEVICE, device).await
    }

    async fn load(&self) -> Result<Option<Device>> {
        let device: Option<Device> = self.get_json(DEVICE, DEVICE).await?;
        Ok(device.map(|mut device| {
            device.device_props =
                Arc::new(whatsapp_rust::wacore::store::device::DEVICE_PROPS.clone());
            device
        }))
    }

    async fn exists(&self) -> Result<bool> {
        Ok(self.vault.exists(DEVICE, DEVICE).await?)
    }

    async fn create(&self) -> Result<i32> {
        let mut tx = self.vault.begin().await?;
        if !tx.exists(DEVICE, DEVICE).await? {
            let value = encode(&Device::new())?;
            tx.write(&[Op::Put(DEVICE, &Row::new(DEVICE, &value))])
                .await?;
        }
        tx.commit().await?;
        Ok(1)
    }
}
