#![cfg(feature = "test-util")]

use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use solti_core::{OutputConfig, TaskState};
use solti_model::{EmbeddedSpec, TaskId, TaskQuery, TaskSpec, TaskWorkload};

fn task_spec() -> TaskSpec {
    TaskSpec::builder(
        "property",
        TaskWorkload::Embedded(EmbeddedSpec::new("property-v1").expect("embedded kind")),
        30_000_u64,
    )
    .build()
    .expect("property-test Task spec")
}

fn task_id(index: usize) -> TaskId {
    TaskId::new(format!("snapshot-{index:04}")).expect("generated property-test Task name")
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource(
            "proptest-regressions",
        ))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn effective_output_capacity_is_the_largest_admissible_power_of_two(
        raw_capacity in any::<usize>(),
        raw_byte_budget in any::<usize>(),
        raw_max_chunk in any::<usize>(),
    ) {
        let capacity = raw_capacity.max(1);
        let byte_budget = raw_byte_budget.max(1);
        let max_chunk_bytes = raw_max_chunk.max(1).min(byte_budget);
        let config = OutputConfig::try_new(capacity)
            .unwrap()
            .try_with_byte_limits(byte_budget, max_chunk_bytes)
            .unwrap();

        let effective = config.effective_capacity().get();
        let upper_bound = capacity
            .min(byte_budget / max_chunk_bytes)
            .min(usize::MAX >> 1);

        prop_assert!(effective.is_power_of_two());
        prop_assert!(effective <= upper_bound);
        prop_assert!(effective.checked_mul(max_chunk_bytes).unwrap() <= byte_budget);
        prop_assert!(effective.checked_mul(2).unwrap() > upper_bound);
    }

    #[test]
    fn pagination_keeps_its_original_snapshot_across_late_interleaved_inserts(
        initial_count in 2usize..48,
        page_seed in any::<usize>(),
        late_count in 1usize..16,
    ) {
        let state = TaskState::new();
        let spec = task_spec();
        let original_names = (0..initial_count)
            .map(|index| task_id(index * 2))
            .collect::<Vec<_>>();
        for name in &original_names {
            state.seed_task(name.clone(), spec.clone());
        }

        let page_limit = page_seed % (initial_count - 1) + 1;
        let query = TaskQuery::new().with_limit(page_limit);
        let mut page = state.query(&query).unwrap();
        let snapshot_version = page.resource_version.clone();

        for index in 0..late_count {
            state.seed_task(task_id(index * 2 + 1), spec.clone());
        }

        let mut observed_names = Vec::with_capacity(initial_count);
        loop {
            prop_assert_eq!(&page.resource_version, &snapshot_version);
            observed_names.extend(page.items.iter().map(|task| task.name().clone()));
            prop_assert_eq!(page.remaining_item_count, initial_count - observed_names.len());

            let Some(continuation) = page.continuation else {
                break;
            };
            page = state
                .query(&query.clone().with_continuation(continuation))
                .unwrap();
        }

        prop_assert_eq!(observed_names, original_names);
    }
}
