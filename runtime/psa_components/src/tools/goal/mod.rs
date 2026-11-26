//! Goal tools are installed together with their shared state and hook by GoalBundle.
mod set_goal;
mod set_goal_status;

pub use set_goal::SetGoal;
pub use set_goal_status::SetGoalStatus;
