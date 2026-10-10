//! An account in memory: for tests, with a size limit and injectable failures.

use std::collections::BTreeMap;

use crate::{CloudError, Listed, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result};

#[derive(Debug, Default)]
pub struct MemProvider {
    label: String,
    objects: BTreeMap<ObjectName, Vec<u8>>,
    capacity: Option<u64>,
    max_file_size: Option<u64>,
    failures: Vec<CloudError>,
}

impl MemProvider {
    pub fn new(label: impl Into<String>) -> MemProvider {
        MemProvider { label: label.into(), ..MemProvider::default() }
    }

    /// Refuse writes that would store more than `bytes` in total.
    pub fn with_capacity(mut self, bytes: u64) -> MemProvider {
        self.capacity = Some(bytes);
        self
    }

    pub fn with_max_file_size(mut self, bytes: u64) -> MemProvider {
        self.max_file_size = Some(bytes);
        self
    }

    /// The next call (any operation but `info`) fails with `err`; queued failures are used in
    /// order.
    pub fn fail_next(&mut self, err: CloudError) {
        self.failures.push(err);
    }

    /// Replace an object's bytes behind the provider's back (simulates damage on the server).
    pub fn tamper(&mut self, name: &ObjectName, data: Vec<u8>) {
        self.objects.insert(name.clone(), data);
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    fn injected(&mut self) -> Result<()> {
        if self.failures.is_empty() { Ok(()) } else { Err(self.failures.remove(0)) }
    }

    fn used(&self) -> u64 {
        self.objects.values().fold(0u64, |n, v| n.saturating_add(v.len() as u64))
    }

    fn name_of(remote: &RemoteRef) -> Result<ObjectName> {
        ObjectName::parse(&remote.key)
    }
}

impl Provider for MemProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: "memory".into(), label: self.label.clone(), max_file_size: self.max_file_size }
    }

    fn quota(&mut self) -> Result<Quota> {
        self.injected()?;
        Ok(Quota { total: self.capacity, used: self.used() })
    }

    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        self.injected()?;
        let size = data.len() as u64;
        if let Some(max) = self.max_file_size.filter(|max| size > *max) {
            return Err(CloudError::TooLarge { size, max });
        }
        let replaced = self.objects.get(name).map_or(0, |v| v.len() as u64);
        if let Some(cap) = self.capacity
            && self.used().saturating_sub(replaced).saturating_add(size) > cap
        {
            return Err(CloudError::QuotaExceeded);
        }
        self.objects.insert(name.clone(), data.to_vec());
        Ok(RemoteRef { key: name.as_str().to_string() })
    }

    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        self.injected()?;
        let data = self.objects.get(&Self::name_of(remote)?).ok_or(CloudError::NotFound)?;
        let size = data.len() as u64;
        if size > max_len {
            return Err(CloudError::TooLarge { size, max: max_len });
        }
        Ok(data.clone())
    }

    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        self.injected()?;
        self.objects.remove(&Self::name_of(remote)?);
        Ok(())
    }

    fn list(&mut self) -> Result<Vec<Listed>> {
        self.injected()?;
        Ok(self
            .objects
            .iter()
            .map(|(name, data)| Listed { name: name.clone(), remote: RemoteRef { key: name.as_str().to_string() }, size: data.len() as u64 })
            .collect())
    }
}
