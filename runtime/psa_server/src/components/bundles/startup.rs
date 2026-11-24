use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::{
    agents::Agent,
    components::hooks::{
        CwdHook, DefaultStartupHook, HookKind, ProjectContextHook, SessionModelUpdateHook,
        SkillsHook, ToolDefinitionsHook,
    },
};
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
            HookKind::DefaultStartup(DefaultStartupHook::new(self.base_prompt)),
        );
        installer.add_hook(
            HookStage::AgentCreate,
            HookKind::ToolDefinitions(ToolDefinitionsHook),
        );
        if self.project_context {
            installer.add_hook(
                HookStage::AgentCreate,
                HookKind::ProjectContext(ProjectContextHook),
            );
        }
        if self.cwd {
            installer.add_hook(HookStage::AgentCreate, HookKind::Cwd(CwdHook));
        }
        if self.skills {
            installer.add_hook(HookStage::AgentCreate, HookKind::Skills(SkillsHook));
        }
        installer.add_hook(
            HookStage::SessionUpdate,
            HookKind::SessionModelUpdate(SessionModelUpdateHook),
        );
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}
