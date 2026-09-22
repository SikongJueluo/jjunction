//! jjunction — tools for the Jujutsu (jj) version control system.
//!
//! Built directly on top of the [`jj-lib`] crate.

pub mod config;

#[cfg(test)]
mod tests {
    use jj_lib::config::ConfigLayer;
    use jj_lib::config::ConfigSource;
    use jj_lib::config::StackedConfig;
    use jj_lib::settings::UserSettings;

    #[test]
    fn builds_user_settings_from_config() {
        let mut config = StackedConfig::with_defaults();
        config.add_layer(
            ConfigLayer::parse(
                ConfigSource::User,
                r#"
                user.name = "Test User"
                user.email = "test.user@example.com"
                operation.username = "test-username"
                operation.hostname = "host.example.com"
                "#,
            )
            .unwrap(),
        );

        let settings = UserSettings::from_config(config).unwrap();

        assert_eq!(settings.user_name(), "Test User");
        assert_eq!(settings.user_email(), "test.user@example.com");
    }
}
