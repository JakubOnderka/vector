/// This pipelines transform is a bit complex and needs a simple example.
///
/// If we consider the following example:
///
/// ```toml
/// [transforms.my_pipelines]
/// type = "pipelines"
/// inputs = ["syslog"]
///
/// [transforms.my_pipelines.logs]
/// order = ["foo", "bar"]
///
/// [transforms.my_pipelines.logs.pipelines.foo]
/// name = "foo pipeline"
///
/// [[transforms.my_pipelines.logs.pipelines.foo.transforms]]
/// # any transform configuration
///
/// [[transforms.my_pipelines.logs.pipelines.foo.transforms]]
/// # any transform configuration
///
/// [transforms.my_pipelines.logs.pipelines.bar]
/// name = "bar pipeline"
///
/// [transforms.my_pipelines.logs.pipelines.bar.transforms]]
/// # any transform configuration
///
/// [transforms.my_pipelines.metrics]
/// order = ["hello", "world"]
///
/// [transforms.my_pipelines.metrics.pipelines.hello]
/// name = "hello pipeline"
///
/// [[transforms.my_pipelines.metrics.pipelines.hello.transforms]]
/// # any transform configuration
///
/// [[transforms.my_pipelines.metrics.pipelines.hello.transforms]]
/// # any transform configuration
///
/// [transforms.my_pipelines.metrics.pipelines.world]
/// name = "world pipeline"
///
/// [[transforms.my_pipelines.metrics.pipelines.world.transforms]]
/// # any transform configuration
/// ```
///
/// The pipelines transform will first expand into 2 parallel transforms for `logs` and
/// `metrics`. A `Noop` transform will be also added to aggregate `logs` and `metrics`
/// into a single transform and to be able to use the transform name (`my_pipelines`) as an input.
///
/// Then the `logs` group of pipelines will be expanded into a `EventFilter` followed by
/// a series `PipelineConfig` via the `EventRouter` transform. At the end, a `Noop` alias is added
/// to be able to refer `logs` as `my_pipelines.logs`.
/// Same thing for the `metrics` group of pipelines.
///
/// Each pipeline will then be expanded into a list of its transforms and at the end of each
/// expansion, a `Noop` transform will be added to use the `pipeline` name as an alias
/// (`my_pipelines.logs.transforms.foo`).
mod filter;
mod router;

use crate::conditions::AnyCondition;
use crate::config::{
    DataType, GenerateConfig, TransformConfig, TransformContext, TransformDescription,
};
use crate::transforms::{noop::Noop, Transform};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use vector_core::config::ComponentKey;

inventory::submit! {
    TransformDescription::new::<PipelinesConfig>("pipelines")
}

/// This represents the configuration of a single pipeline, not the pipelines transform
/// itself, which can contain multiple individual pipelines
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PipelineConfig {
    name: String,
    filter: Option<AnyCondition>,
    transforms: Vec<Box<dyn TransformConfig>>,
}

#[cfg(test)]
impl PipelineConfig {
    pub fn transforms(&self) -> &Vec<Box<dyn TransformConfig>> {
        &self.transforms
    }
}

impl Clone for PipelineConfig {
    fn clone(&self) -> Self {
        // This is a hack around the issue of cloning
        // trait objects. So instead to clone the config
        // we first serialize it into JSON, then back from
        // JSON. Originally we used TOML here but TOML does not
        // support serializing `None`.
        let json = serde_json::to_value(self).unwrap();
        serde_json::from_value(json).unwrap()
    }
}

impl PipelineConfig {
    fn expand(
        &mut self,
        component_key: &ComponentKey,
        inputs: &[String],
    ) -> crate::Result<Option<IndexMap<ComponentKey, (Vec<String>, Box<dyn TransformConfig>)>>>
    {
        let mut map: IndexMap<ComponentKey, (Vec<String>, Box<dyn TransformConfig>)> =
            IndexMap::new();

        let mut previous: Vec<String> = inputs.into();

        if let Some(ref filter) = self.filter {
            let filter_key = component_key.join("filter");
            map.insert(
                filter_key.clone(),
                (
                    previous.clone(),
                    Box::new(filter::PipelineFilterConfig::new(filter.clone())),
                ),
            );
            previous = vec![filter_key.join("truthy").id().to_owned()];
        }

        for (index, transform) in self.transforms.iter_mut().enumerate() {
            let transform_key = component_key.join(index);
            if let Some(expanded) = transform.expand(&transform_key, &previous)? {
                previous = vec![transform_key.id().to_owned()];
                map.extend(expanded);
            } else {
                map.insert(transform_key, (previous.clone(), transform.clone()));
            }
        }

        if self.filter.is_some() {
            previous.push(component_key.join("filter").join("falsy").id().to_owned());
        } else {
            map.insert(component_key.clone(), (previous, Box::new(Noop)));
        }

        Ok(Some(map))
    }
}

/// This represent an ordered list of pipelines depending on the event type.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventTypeConfig {
    #[serde(default)]
    order: Option<Vec<String>>,
    pipelines: IndexMap<String, PipelineConfig>,
}

#[cfg(test)]
impl EventTypeConfig {
    pub const fn order(&self) -> &Option<Vec<String>> {
        &self.order
    }

    pub const fn pipelines(&self) -> &IndexMap<String, PipelineConfig> {
        &self.pipelines
    }
}

impl EventTypeConfig {
    fn names(&self) -> Vec<String> {
        if let Some(ref names) = self.order {
            // This assumes all the pipelines are present in the `order` field.
            // If a pipeline is missing, it won't be used.
            names.clone()
        } else {
            let mut names = self.pipelines.keys().cloned().collect::<Vec<String>>();
            names.sort();
            names
        }
    }

    fn expand(
        &mut self,
        component_key: &ComponentKey,
        inputs: &[String],
    ) -> crate::Result<Option<IndexMap<ComponentKey, (Vec<String>, Box<dyn TransformConfig>)>>>
    {
        let mut map: IndexMap<ComponentKey, (Vec<String>, Box<dyn TransformConfig>)> =
            IndexMap::new();

        let mut previous: Vec<String> = inputs.into();
        for name in self.names() {
            if let Some(pipeline) = self.pipelines.get_mut(&name) {
                let pipeline_key = component_key.join(name);
                if let Some(expanded) = pipeline.expand(&pipeline_key, &previous)? {
                    map.extend(expanded);
                    previous = vec![pipeline_key.id().to_owned()];
                }
            }
        }

        map.insert(component_key.clone(), (previous, Box::new(Noop)));

        Ok(Some(map))
    }
}

/// The configuration of the pipelines transform itself.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PipelinesConfig {
    #[serde(default)]
    logs: EventTypeConfig,
    #[serde(default)]
    metrics: EventTypeConfig,
}

#[cfg(test)]
impl PipelinesConfig {
    pub const fn logs(&self) -> &EventTypeConfig {
        &self.logs
    }

    pub const fn metrics(&self) -> &EventTypeConfig {
        &self.metrics
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "pipelines")]
impl TransformConfig for PipelinesConfig {
    async fn build(&self, _context: &TransformContext) -> crate::Result<Transform> {
        Err("this transform must be expanded".into())
    }

    /// Expands the pipelines in multiple components
    ///
    /// `id.router`: dispatch function to dispatch logs and events depending on type
    /// `id.router.logs`: output of the dispatch function for logs
    /// `id.router.metrics`: output of the dispatch function fo metrics
    /// `id.logs`: id of the unexpanded transform for logs
    /// `id.metrics`: id of the unexpanded transform for metrics
    /// `id`: noop transform to join metrics and logs stream
    fn expand(
        &mut self,
        component_key: &ComponentKey,
        inputs: &[String],
    ) -> crate::Result<Option<IndexMap<ComponentKey, (Vec<String>, Box<dyn TransformConfig>)>>>
    {
        let mut map: IndexMap<ComponentKey, (Vec<String>, Box<dyn TransformConfig>)> =
            IndexMap::new();
        let router_key = component_key.join("router");
        map.insert(
            router_key.clone(),
            (
                inputs.into(),
                Box::new(router::EventRouterConfig::default()),
            ),
        );
        let mut outputs = Vec::with_capacity(2);

        let logs_inputs = vec![router_key.join("logs").id().to_owned()];
        let logs_key = component_key.join("logs");
        if let Some(expanded) = self.logs.expand(&logs_key, &logs_inputs)? {
            map.extend(expanded);
            outputs.push(logs_key.id().to_owned());
        }

        let metrics_inputs = vec![router_key.join("metrics").id().to_owned()];
        let metrics_key = component_key.join("metrics");
        if let Some(expanded) = self.metrics.expand(&metrics_key, &metrics_inputs)? {
            map.extend(expanded);
            outputs.push(metrics_key.id().to_owned());
        }

        map.insert(component_key.clone(), (outputs, Box::new(Noop)));

        Ok(Some(map))
    }

    fn input_type(&self) -> DataType {
        DataType::Any
    }

    fn output_type(&self) -> DataType {
        DataType::Any
    }

    fn transform_type(&self) -> &'static str {
        "pipelines"
    }

    /// The pipelines transform shouldn't be embedded in another pipelines transform.
    fn nestable(&self, parents: &HashSet<&'static str>) -> Result<(), String> {
        if parents.contains(&self.transform_type()) {
            Err("Pipelines transform shouldn't be nested in a pipelines transform.".to_owned())
        } else {
            let mut nodes = parents.clone();
            nodes.insert(self.transform_type());
            for pipeline in self.logs.pipelines.values() {
                for transform in pipeline.transforms.iter() {
                    transform.nestable(&nodes)?;
                }
            }
            Ok(())
        }
    }
}

impl GenerateConfig for PipelinesConfig {
    fn generate_config() -> toml::Value {
        toml::from_str(indoc::indoc! {r#"
            [logs]
            order = ["foo", "bar"]

            [logs.pipelines.foo]
            name = "foo pipeline"

            [logs.pipelines.foo.filter]
            type = "datadog_search"
            source = "source:s3"

            [[logs.pipelines.foo.transforms]]
            type = "filter"
            condition = ""

            [[logs.pipelines.foo.transforms]]
            type = "filter"
            condition = ""

            [logs.pipelines.bar]
            name = "bar pipeline"

            [[logs.pipelines.bar.transforms]]
            type = "filter"
            condition = ""
        "#})
        .unwrap()
    }
}

#[cfg(test)]
impl PipelinesConfig {
    pub fn from_toml(input: &str) -> Self {
        crate::config::format::deserialize(input, Some(crate::config::format::Format::Toml))
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::{GenerateConfig, PipelinesConfig};
    use crate::config::ComponentKey;
    use vector_core::transform::TransformConfig;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<PipelinesConfig>();
    }

    #[test]
    fn parsing() {
        let config = PipelinesConfig::generate_config();
        let config: PipelinesConfig = config.try_into().unwrap();
        assert_eq!(config.logs.pipelines.len(), 2);
        let foo = config.logs.pipelines.get("foo").unwrap();
        assert_eq!(foo.transforms.len(), 2);
        let bar = config.logs.pipelines.get("bar").unwrap();
        assert_eq!(bar.transforms.len(), 1);
    }

    #[test]
    fn expanding() {
        let config = PipelinesConfig::generate_config();
        let mut config: PipelinesConfig = config.try_into().unwrap();
        let inputs = vec!["syslog".to_owned()];
        let name = ComponentKey::from("foo");
        let expanded = config.expand(&name, &inputs).unwrap().unwrap();
        assert_eq!(
            expanded
                .keys()
                .map(|key| key.to_string())
                .collect::<Vec<String>>(),
            vec![
                "foo.router",
                "foo.logs.foo.filter",
                "foo.logs.foo.0",
                "foo.logs.foo.1",
                "foo.logs.bar.0",
                "foo.logs.bar",
                "foo.logs",
                "foo.metrics",
                "foo"
            ],
        );
    }
}
