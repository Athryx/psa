use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::hooks::{
    CwdHook, DefaultStartupHook, ProjectContextHook, SessionModelUpdateHook, SkillsHook,
    ToolDefinitionsHook,
};
use psa_agents::Agent;
use psa_store::Session;

/// Initializes a new root agent's context and keeps its model following the
/// session. Owns no resources.
///
/// Must be listed before every other bundle: hooks run in bundle order, and
/// `DefaultStartup`'s `NewContext` clears context written by earlier creation
/// hooks.
pub struct StartupBundle {
    base_prompt: bool,
    project_context: bool,
    cwd: bool,
    skills: bool,
}

impl StartupBundle {
    pub fn new(base_prompt: bool, project_context: bool, cwd: bool, skills: bool) -> Self {
        Self {
            base_prompt,
            project_context,
            cwd,
            skills,
        }
    }
}

impl Bundle for StartupBundle {
    fn label(&self) -> String {
        "Startup".into()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        // Startup and tool definitions skip forks, which inherit them; the optional
        // context reflects a fork's own working directory.
        installer.add_hook(
            HookStage::AgentCreate,
            DefaultStartupHook::new(self.base_prompt),
        );
        installer.add_hook(HookStage::AgentCreate, ToolDefinitionsHook);
        if self.project_context {
            installer.add_hook(HookStage::AgentCreate, ProjectContextHook);
        }
        if self.cwd {
            installer.add_hook(HookStage::AgentCreate, CwdHook);
        }
        if self.skills {
            installer.add_hook(HookStage::AgentCreate, SkillsHook);
        }
        installer.add_hook(HookStage::SessionUpdate, SessionModelUpdateHook);
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}
