use rudder_engine::journal::Journal;

#[test]
fn current_attempt_getter_never_observes_or_replaces_an_instance() {
    let temporary = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(temporary.path().join("journal.sqlite")).unwrap();
    assert_eq!(journal.database_path().unwrap(), temporary.path().join("journal.sqlite"));
    let owner = journal.create_project("project", "owner", "base", 0).unwrap();
    journal.register_run(&owner, "run", 0).unwrap();
    assert!(journal.current_run_attempt(&owner, "run", 1).is_err());
    let epoch = journal.observe_run_attempt(&owner, "run", "first", 1).unwrap();
    let sequence = journal.project_state("project").unwrap().sequence;
    assert_eq!(journal.current_run_attempt(&owner, "run", epoch).unwrap(), "first");
    assert_eq!(journal.project_state("project").unwrap().sequence, sequence);
    let next = journal.observe_run_attempt(&owner, "run", "replacement", 2).unwrap();
    assert!(journal.current_run_attempt(&owner, "run", epoch).is_err());
    assert_eq!(journal.current_run_attempt(&owner, "run", next).unwrap(), "replacement");
    journal.transfer_owner(&owner, "new-owner", 3).unwrap();
    assert!(journal.current_run_attempt(&owner, "run", next).is_err());
}
