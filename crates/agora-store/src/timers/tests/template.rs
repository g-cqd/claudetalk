//! The template checks that depend on the creator at `timer.create`: the weight floor (S-7) and the path breadth (S-8).

use super::*;

#[test]
fn template_weight_below_the_floor_is_refused() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    for weight in [0.05, 0.0999] {
        let light = AddArgs {
            weight,
            ..template("a")
        };
        let err = create(&mut f, &alice, Some("light"), at(NOW + HOUR), light).unwrap_err();
        assert!(
            matches!(err, StoreError::Invalid("weight must be at least 0.1")),
            "weight {weight}: {err}"
        );
    }
    assert!(f.s.timers(None).unwrap().is_empty());
    let floor = AddArgs {
        weight: 0.1,
        ..template("a")
    };
    create(&mut f, &alice, Some("floor"), at(NOW + HOUR), floor).expect("0.1 is the floor");
    assert_eq!(f.s.timers(None).unwrap().len(), 1);
}

#[test]
fn template_files_too_broad_for_a_worker_are_refused_and_a_supervisor_may_name_them() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let sup = Identity::from_seed([2; 32]);
    for file in ["/", "/x"] {
        let broad = AddArgs {
            files: vec![file.to_string()],
            ..template("a")
        };
        match create(&mut f, &alice, Some("broad"), at(NOW + HOUR), broad) {
            Err(StoreError::Refused(message)) => {
                assert_eq!(message, format!("{file} is too broad to claim"));
            }
            other => panic!("{file}: expected a refusal, got {other:?}"),
        }
    }
    assert!(f.s.timers(None).unwrap().is_empty());
    let root = AddArgs {
        files: vec!["/".to_string()],
        ..template("a")
    };
    let rec = create(&mut f, &sup, Some("root"), at(NOW + HOUR), root).expect("supervisor");
    assert_eq!(rec.template.files, vec!["/".to_string()]);
}
