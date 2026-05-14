//! Credential resolution chain.

use pi_core::PiResult;

pub trait Resolver: Send + Sync {
    fn lookup(&self, provider: &str, env_name: &str) -> PiResult<Option<String>>;
    fn store(&mut self, provider: &str, env_name: &str, value: &str) -> PiResult<()>;
    fn delete(&mut self, provider: &str, env_name: &str) -> PiResult<bool>;
    fn list(&self) -> PiResult<Vec<String>>;
}

#[derive(Debug, Default)]
pub struct EnvResolver;

impl Resolver for EnvResolver {
    fn lookup(&self, _provider: &str, env_name: &str) -> PiResult<Option<String>> {
        Ok(std::env::var(env_name).ok())
    }
    fn store(&mut self, _provider: &str, _env_name: &str, _value: &str) -> PiResult<()> {
        Ok(())
    }
    fn delete(&mut self, _provider: &str, _env_name: &str) -> PiResult<bool> {
        Ok(false)
    }
    fn list(&self) -> PiResult<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Walks a chain of resolvers. The first one to return `Some` wins for
/// lookups. Writes go to the *last* mutable backend, since the env resolver
/// is read-only by design.
pub struct LayeredResolver {
    layers: Vec<Box<dyn Resolver>>,
}

impl LayeredResolver {
    pub fn new(layers: Vec<Box<dyn Resolver>>) -> Self {
        Self { layers }
    }

    pub fn writable_index(&self) -> Option<usize> {
        // last layer is the writable one in our default stack.
        if self.layers.is_empty() {
            None
        } else {
            Some(self.layers.len() - 1)
        }
    }
}

impl Resolver for LayeredResolver {
    fn lookup(&self, provider: &str, env_name: &str) -> PiResult<Option<String>> {
        for layer in &self.layers {
            if let Some(value) = layer.lookup(provider, env_name)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    fn store(&mut self, provider: &str, env_name: &str, value: &str) -> PiResult<()> {
        if let Some(idx) = self.writable_index() {
            self.layers[idx].store(provider, env_name, value)
        } else {
            Ok(())
        }
    }

    fn delete(&mut self, provider: &str, env_name: &str) -> PiResult<bool> {
        let mut any = false;
        for layer in &mut self.layers {
            if layer.delete(provider, env_name)? {
                any = true;
            }
        }
        Ok(any)
    }

    fn list(&self) -> PiResult<Vec<String>> {
        let mut out = Vec::new();
        for layer in &self.layers {
            for name in layer.list()? {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Build the default `env -> [keyring ->] encrypted-file` chain.
pub fn layered_resolver() -> PiResult<LayeredResolver> {
    let mut layers: Vec<Box<dyn Resolver>> = Vec::new();
    layers.push(Box::new(EnvResolver));
    #[cfg(feature = "keyring")]
    {
        if let Ok(store) = crate::keyring_store::KeyringStore::new() {
            layers.push(Box::new(store));
        }
    }
    #[cfg(feature = "encrypted-file")]
    {
        let path = crate::default_auth_path()?;
        layers.push(Box::new(crate::encrypted_file::EncryptedFileStore::open(
            path,
        )?));
    }
    Ok(LayeredResolver::new(layers))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticResolver(Vec<(String, String)>);
    impl Resolver for StaticResolver {
        fn lookup(&self, _: &str, env_name: &str) -> PiResult<Option<String>> {
            Ok(self
                .0
                .iter()
                .find(|(k, _)| k == env_name)
                .map(|(_, v)| v.clone()))
        }
        fn store(&mut self, _: &str, env_name: &str, value: &str) -> PiResult<()> {
            self.0.retain(|(k, _)| k != env_name);
            self.0.push((env_name.to_string(), value.to_string()));
            Ok(())
        }
        fn delete(&mut self, _: &str, env_name: &str) -> PiResult<bool> {
            let before = self.0.len();
            self.0.retain(|(k, _)| k != env_name);
            Ok(self.0.len() != before)
        }
        fn list(&self) -> PiResult<Vec<String>> {
            Ok(self.0.iter().map(|(k, _)| k.clone()).collect())
        }
    }

    #[test]
    fn first_layer_with_value_wins() {
        let mut layered = LayeredResolver::new(vec![
            Box::new(StaticResolver(vec![(
                "MOONSHOT_API_KEY".to_string(),
                "top".to_string(),
            )])),
            Box::new(StaticResolver(vec![(
                "MOONSHOT_API_KEY".to_string(),
                "bottom".to_string(),
            )])),
        ]);
        let v = layered.lookup("moonshot", "MOONSHOT_API_KEY").unwrap();
        assert_eq!(v.as_deref(), Some("top"));
        // store writes to the writable (last) layer
        layered.store("x", "X_API_KEY", "v").unwrap();
        let names = layered.list().unwrap();
        assert!(names.contains(&"X_API_KEY".to_string()));
    }
}
