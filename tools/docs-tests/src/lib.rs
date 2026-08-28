//! Compile-only owner for runnable examples in the repository-level SDK documentation.

#![forbid(unsafe_code)]

macro_rules! markdown_doctest {
    ($name:ident, $path:literal) => {
        #[cfg(all(doctest, feature = "guides"))]
        #[doc = include_str!($path)]
        struct $name;
    };
}

markdown_doctest!(WorkspaceReadme, "../../../README.md");
markdown_doctest!(GuideIndex, "../../../docs/index.md");
markdown_doctest!(QuickStartGuide, "../../../docs/quick-start.md");
markdown_doctest!(MentalModelGuide, "../../../docs/mental-model.md");
markdown_doctest!(InstallationGuide, "../../../docs/installation.md");
markdown_doctest!(ArchitectureGuide, "../../../docs/architecture.md");
markdown_doctest!(BuildingAgentGuide, "../../../docs/building-an-agent.md");
markdown_doctest!(RoutingGuide, "../../../docs/routing-and-custom-runners.md");
markdown_doctest!(SubprocessGuide, "../../../docs/subprocesses.md");
markdown_doctest!(ContainersGuide, "../../../docs/containers-and-isolation.md");
markdown_doctest!(ChainsGuide, "../../../docs/chains.md");
markdown_doctest!(TaskResourcesGuide, "../../../docs/task-resources.md");
markdown_doctest!(ManagingTasksGuide, "../../../docs/managing-tasks.md");
markdown_doctest!(ReconciliationGuide, "../../../docs/reconciliation.md");
markdown_doctest!(LifecycleGuide, "../../../docs/lifecycle-and-admission.md");
markdown_doctest!(CollectionsGuide, "../../../docs/collections-and-watches.md");
markdown_doctest!(OutputGuide, "../../../docs/output-and-history.md");
markdown_doctest!(
    CancellationGuide,
    "../../../docs/cancellation-and-shutdown.md"
);
markdown_doctest!(ServingApiGuide, "../../../docs/serving-api.md");
markdown_doctest!(DiscoveryGuide, "../../../docs/discovery.md");
markdown_doctest!(TlsGuide, "../../../docs/tls-and-authentication.md");
markdown_doctest!(ConfigurationGuide, "../../../docs/configuration.md");
markdown_doctest!(ObservabilityGuide, "../../../docs/observability.md");
markdown_doctest!(PersistenceGuide, "../../../docs/persistence.md");
markdown_doctest!(
    ProductionBoundariesGuide,
    "../../../docs/production-boundaries.md"
);
markdown_doctest!(CommonMistakesGuide, "../../../docs/common-mistakes.md");
markdown_doctest!(ExampleCatalogGuide, "../../../docs/example-catalog.md");
markdown_doctest!(ApiReferenceGuide, "../../../docs/api-reference.md");
