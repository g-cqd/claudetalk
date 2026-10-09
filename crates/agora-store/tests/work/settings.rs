use super::*;

// ---- settings --------------------------------------------------------------------------------------------------

#[test]
fn changing_the_settings_needs_a_controller() {
    let mut w = world();
    let (s, sup) = (&mut w.s, &w.sup);
    let arg = json!({"patch": {"self_accept": "true"}}).to_string();
    let err = s.work_settings_set(NOW, &op(sup, "work.settings", "", &arg));
    assert_eq!(forbidden(err), Role::Supervisor);
}

#[test]
fn a_settings_patch_is_stored_and_read_back() {
    let mut w = world();
    let ctl = controller(&mut w);
    let arg = json!({"patch": {"self_accept": "true"}}).to_string();
    let returned =
        w.s.work_settings_set(NOW, &op(&ctl, "work.settings", "", &arg))
            .expect("settings");
    assert!(returned.self_accept);
    assert!(w.s.work_settings().expect("read").self_accept);
}

#[test]
fn an_invalid_settings_value_is_refused_and_nothing_is_stored() {
    let mut w = world();
    let ctl = controller(&mut w);
    let arg = json!({"patch": {"self_accept": "maybe"}}).to_string();
    let err =
        w.s.work_settings_set(NOW, &op(&ctl, "work.settings", "", &arg));
    assert!(matches!(err, Err(StoreError::Refused(_))), "{err:?}");
    assert!(!w.s.work_settings().expect("read").self_accept);
}
