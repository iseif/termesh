//! Schema-version machinery shared by the files this crate owns (ADR-0014 §2).
//!
//! One `CURRENT_VERSION` per file, and a migrate step per version transition, chained.
//! There are no transitions yet — the first one lands here, ahead of whatever needs the
//! version bump — so today `migrate` is the identity function plus a version stamp. The
//! point is that the machinery exists and is tested before it is needed.

use crate::settings::Settings;

/// Migrate `settings` up to [`Settings::CURRENT_VERSION`], in memory, from whatever
/// older version it declares. Idempotent by construction (ADR-0014 §2): applying it
/// twice must equal applying it once, since the caller does not track how many times a
/// value has already been migrated.
pub fn migrate(mut settings: Settings) -> Settings {
    settings.version = Settings::CURRENT_VERSION;
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_idempotent() {
        let once = migrate(Settings::parse_raw("tab_width = 2\n"));
        let twice = migrate(once.clone());
        assert_eq!(once, twice);
    }
}
