use super::RoutedBackend;

#[derive(Clone, Debug)]
pub enum Deployment {
    Single { backend: RoutedBackend },
}

impl Deployment {
    pub fn primary_backend(&self) -> &RoutedBackend {
        match self {
            Self::Single { backend } => backend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_backend() -> RoutedBackend {
        RoutedBackend {
            name: "citadel".to_string(),
            url: "http://citadel:11434".to_string(),
        }
    }

    #[test]
    fn single_primary_backend_returns_contained_backend() {
        let backend = sample_backend();
        let deployment = Deployment::Single {
            backend: backend.clone(),
        };

        let primary = deployment.primary_backend();
        assert_eq!(primary.name, backend.name);
        assert_eq!(primary.url, backend.url);
    }

    #[test]
    fn single_is_cloneable() {
        let deployment = Deployment::Single {
            backend: sample_backend(),
        };
        let clone = deployment.clone();
        assert_eq!(clone.primary_backend().name, "citadel");
    }
}
