use super::*;

impl Registry {
    pub fn verifier_configured(&self) -> bool {
        std::env::var("MINK_GOAL_CHECK")
            .ok()
            .filter(|c| !c.trim().is_empty())
            .is_some_and(|command| mink::runtime::CommandGoalVerifier::new(command, 60).is_ok())
    }

    pub(super) fn build_options(
        &self,
        session_ref: &str,
        cwd: &Path,
    ) -> anyhow::Result<AgentOptions> {
        use crate::session::agent_config;

        // Layered merge mirroring the CLI: project .minkrc overrides user
        // ~/.minkrc; server env vars override both (documented server
        // precedence: env > mink-server.toml > project .minkrc > user .minkrc).
        let mut cfg = agent_config::merge(
            self.agent_layer.clone(),
            agent_config::load_project_layer(cwd)?,
        );
        agent_config::apply_env_overrides(&mut cfg);
        let model = cfg.model.clone().unwrap_or_else(|| self.model.clone());

        let mut options = AgentOptions::new(&self.home, cwd)
            .with_session(SessionPolicy::UseOrCreate(session_ref.to_string()));
        options = agent_config::apply_to(options, &cfg);
        // Re-apply identity fields after `with_provider_options` replaced them
        // with defaults.
        options = options.with_model(&model);
        if let Some(key) = &cfg.api_key {
            options = options.with_api_key(key);
        }
        if let Some(url) = &cfg.base_url {
            options = options.with_base_url(url);
        }
        options = options
            .with_project_scoped_sessions()
            .with_log_events(cfg.log_events.unwrap_or(true));
        // Checker acceptance needs a verifier. The terminal reads MINK_GOAL_CHECK;
        // without the same wiring the workbench would offer an acceptance mode
        // the server cannot honour.
        if let Ok(command) = std::env::var("MINK_GOAL_CHECK")
            && !command.trim().is_empty()
        {
            match mink::runtime::CommandGoalVerifier::new(command, 60) {
                Ok(verifier) => {
                    options = options.with_goal_verifier(std::sync::Arc::new(verifier));
                }
                // A bad command disables checker acceptance rather than failing
                // session startup; the capability is reported as unavailable.
                Err(error) => {
                    eprintln!("MINK_GOAL_CHECK ignored: {error:#}");
                }
            }
        }
        if let Some(backend) = &self.llm_backend {
            options = options.with_llm_backend(backend.clone());
        }
        Ok(options)
    }
}
