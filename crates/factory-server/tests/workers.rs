use factory_server::WorkerSlots;

fn numbers(slots: &WorkerSlots) -> Vec<(usize, i64)> {
    slots
        .busy()
        .iter()
        .map(|slot| (slot.number, slot.job_id))
        .collect()
}

#[test]
fn a_job_takes_the_lowest_free_slot_number() {
    let slots = WorkerSlots::new();
    slots.take(10, 100);
    slots.take(11, 101);
    assert_eq!(numbers(&slots), [(1, 10), (2, 11)]);

    slots.free(10);
    assert_eq!(numbers(&slots), [(2, 11)]);
    slots.take(12, 102);
    assert_eq!(numbers(&slots), [(1, 12), (2, 11)]);
}

#[test]
fn taking_twice_keeps_the_first_slot_and_start_time() {
    let slots = WorkerSlots::new();
    slots.take(10, 100);
    slots.take(10, 200);
    let busy = slots.busy();
    assert_eq!(busy.len(), 1);
    assert_eq!(busy[0].number, 1);
    assert_eq!(busy[0].since, 100);
}

#[test]
fn freeing_a_job_without_a_slot_changes_nothing() {
    let slots = WorkerSlots::new();
    slots.take(10, 100);
    slots.free(99);
    assert_eq!(numbers(&slots), [(1, 10)]);
}
