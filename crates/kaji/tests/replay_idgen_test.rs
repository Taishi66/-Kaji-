use kaji::replay::idgen::{default_idgen, IdGen, SessionIdGen};

#[test]
fn same_seed_and_turn_yield_the_same_id_sequence() {
    let a = SessionIdGen::new("sess-1");
    let b = SessionIdGen::new("sess-1");
    a.begin_turn("sess-1", 1);
    b.begin_turn("sess-1", 1);
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
    a.begin_turn("sess-1", 1);
    b.begin_turn("sess-2", 1);
    assert_ne!(a.next_message_id(), b.next_message_id());
}

/// Le compteur repart à chaque tour, sous une graine qui porte le tour : une
/// session reprise dans un autre processus recommence à zéro sans redonner les
/// ids de son premier tour.
#[test]
fn each_turn_has_its_own_id_space() {
    let recorded = SessionIdGen::new("sess-1");
    recorded.begin_turn("sess-1", 1);
    let first_turn: Vec<_> = (0..2).map(|_| recorded.next_message_id()).collect();
    recorded.begin_turn("sess-1", 2);
    let second_turn: Vec<_> = (0..2).map(|_| recorded.next_message_id()).collect();

    assert!(
        first_turn.iter().all(|id| !second_turn.contains(id)),
        "{first_turn:?} vs {second_turn:?}"
    );

    let replayed = SessionIdGen::new("sess-1");
    replayed.begin_turn("replay-of-sess-1", 2);
    let replayed: Vec<_> = (0..2).map(|_| replayed.next_message_id()).collect();
    assert_eq!(
        second_turn, replayed,
        "la graine imposée par le rejeu l'emporte sur la session dérivée"
    );
}

/// L'enregistrement : la graine vient de la session du tour, donc le journal
/// porte les ids que le rejeu redérivera (spec S1).
#[test]
fn a_recording_gen_adopts_the_seed_of_its_session() {
    let recorded = SessionIdGen::per_session();
    recorded.begin_turn("sess-1", 3);
    let recorded: Vec<_> = (0..2).map(|_| recorded.next_message_id()).collect();

    let replayed = SessionIdGen::new("sess-1");
    replayed.begin_turn("replay-of-sess-1", 3);
    let replayed: Vec<_> = (0..2).map(|_| replayed.next_message_id()).collect();

    assert_eq!(recorded, replayed);
}

/// Hors tour, deux agents du même processus ne doivent pas partager une suite
/// d'ids : la graine reste tirée au hasard jusqu'au premier `begin_turn`.
#[test]
fn two_gens_do_not_collide_before_a_turn_opens() {
    let a = default_idgen();
    let b = default_idgen();
    assert_ne!(a.next_message_id(), b.next_message_id());
}
