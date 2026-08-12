use grove::load::{self, Load};

/// The definition of "too busy": more runnable work than there are cores to run it.
/// Expressed against core count rather than a fixed number, because load 26 is a crisis
/// on a laptop and a quiet afternoon on a build server.
#[test]
fn a_machine_is_oversubscribed_when_load_reaches_its_core_count() {
    assert!(
        !Load {
            one: 15.9,
            cores: 16
        }
        .oversubscribed()
    );
    assert!(
        Load {
            one: 16.0,
            cores: 16
        }
        .oversubscribed()
    );
    assert!(
        Load {
            one: 26.1,
            cores: 16
        }
        .oversubscribed()
    );
}

/// The false positive this whole feature has to avoid. Two instances on a sixteen-core
/// machine cross load 16 the moment one of them runs a big type-check — but there is
/// nothing worth reclaiming, so a warning is pure noise and the reader learns to skip
/// every grove warning after it.
#[test]
fn a_busy_machine_with_few_instances_is_not_worth_warning_about() {
    let hammered = Load {
        one: 30.0,
        cores: 8,
    };

    assert!(!load::should_warn(Some(&hammered), 1));
    assert!(!load::should_warn(Some(&hammered), 2));
    assert!(!load::should_warn(Some(&hammered), 3));
    assert!(load::should_warn(Some(&hammered), 4));
}

/// The other half: a crowd on a machine that is coping fine is not a problem either.
#[test]
fn a_crowd_on_an_idle_machine_is_not_worth_warning_about() {
    let calm = Load {
        one: 1.2,
        cores: 16,
    };
    assert!(!load::should_warn(Some(&calm), 13));
}

/// `getloadavg` can fail, and a machine grove cannot measure is not a machine grove
/// should make claims about.
#[test]
fn an_unreadable_load_never_warns() {
    assert!(!load::should_warn(None, 13));
}
