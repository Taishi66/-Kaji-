use kaji::replay::idgen::{IdGen, SessionIdGen};

#[test]
fn same_seed_yields_same_id_sequence() {
    let a = SessionIdGen::new("sess-1");
    let b = SessionIdGen::new("sess-1");
    let ids_a: Vec<_> = (0..3).map(|_| a.next_message_id()).collect();
    let ids_b: Vec<_> = (0..3).map(|_| b.next_message_id()).collect();
    assert_eq!(ids_a, ids_b);
    assert_eq!(ids_a.len(), 3);
    assert!(ids_a[0].starts_with("msg_"));
    assert_ne!(ids_a[0], ids_a[1]);
}

#[test]
fn different_seeds_diverge() {
    let a = SessionIdGen::new("sess-1");
    let b = SessionIdGen::new("sess-2");
    assert_ne!(a.next_message_id(), b.next_message_id());
}
