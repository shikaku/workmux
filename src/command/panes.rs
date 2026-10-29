//! `workmux panes`: switch a window to a layout, opening the panes it lacks
//! and closing the ones only other layouts have.
//!
//! A layout's panes are matched against the window's: the agent pane by the
//! agent state workmux tracks for it, any other pane by its `name` (the
//! `@workmux_pane_name` option `set_pane_name` writes, or the pane title). A
//! missing pane is split from the pane the layout splits it from, or, when
//! only a pane the layout split from it is there, before that pane. Panes
//! already open, and their commands, are left as they are; an agent pane is
//! never started. A pane named after a pane of some layout of the config
//! that the chosen layout has no place for is closed; an agent pane and a
//! pane no layout names never are.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{self, Config, PaneConfig, SandboxTarget, SplitDirection};
use crate::multiplexer::{Multiplexer, WindowPane, create_backend, detect_backend, util};
use crate::state::StateStore;

/// The layout name that stands for the top-level `panes`.
const DEFAULT_LAYOUT: &str = "default";
const SIDEBAR_ROLE: &str = "sidebar";

/// How a layout pane is told among the window's panes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    Agent,
    Named(String),
    /// Neither an agent nor named: nothing tells whether it is open.
    Unknown,
}

/// One step that opens a missing pane, by index into the layout's panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Split `target` as the layout does.
    After { pane: usize, target: usize },
    /// Split `child` and put the new pane before it: the layout split `child`
    /// from `pane`, and only `child` is there.
    Before { pane: usize, child: usize },
}

/// The steps that open the missing creatable panes, and the ones no step can
/// place. `targets[k]` is the pane pane `k` splits from (None for the first).
pub(crate) fn plan(
    targets: &[Option<usize>],
    present: &[bool],
    creatable: &[bool],
) -> (Vec<Step>, Vec<usize>) {
    let mut have = present.to_vec();
    let mut steps = Vec::new();
    loop {
        let mut progressed = false;
        for pane in 0..targets.len() {
            if have[pane] || !creatable[pane] {
                continue;
            }
            if let Some(target) = targets[pane].filter(|&t| have[t]) {
                steps.push(Step::After { pane, target });
            } else if let Some(child) =
                (0..targets.len()).find(|&c| targets[c] == Some(pane) && have[c])
            {
                steps.push(Step::Before { pane, child });
            } else {
                continue;
            }
            have[pane] = true;
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    let unplaced = (0..targets.len())
        .filter(|&pane| !have[pane] && creatable[pane])
        .collect();

    (steps, unplaced)
}

/// The size of a pane put before `child`, so that `child` keeps the size the
/// layout gave it: a percentage's complement, or what is left of `extent`
/// (the child's lines or columns along the split, one for the border).
/// None for both lets the multiplexer halve it.
pub(crate) fn before_size(child: &PaneConfig, extent: Option<u16>) -> (Option<u16>, Option<u8>) {
    if let Some(percentage) = child.percentage {
        return (None, Some(100u8.saturating_sub(percentage).max(1)));
    }
    match (child.size, extent) {
        (Some(size), Some(extent)) if extent > size + 1 => (Some(extent - size - 1), None),
        _ => (None, None),
    }
}

/// The panes `setup_panes` creates, with the index each splits from: a pane
/// after the first without `split` is skipped there, and `target` counts the
/// created ones. A target that is not an earlier pane is None.
fn effective_panes(panes: &[PaneConfig]) -> Vec<(&PaneConfig, Option<usize>)> {
    let created: Vec<&PaneConfig> = panes
        .iter()
        .enumerate()
        .filter(|(i, pane)| *i == 0 || pane.split.is_some())
        .map(|(_, pane)| pane)
        .collect();

    created
        .iter()
        .enumerate()
        .map(|(k, pane)| {
            let target = (k > 0)
                .then(|| pane.target.unwrap_or(k - 1))
                .filter(|&t| t < k);
            (*pane, target)
        })
        .collect()
}

fn layout_panes(config: &Config, layout: Option<&str>) -> Result<Vec<PaneConfig>> {
    let named = layout.and_then(|name| config.layouts.as_ref()?.get(name));
    if let Some(found) = named {
        return Ok(found.panes.clone());
    }
    match layout {
        Some(name) if name != DEFAULT_LAYOUT => {
            let mut available: Vec<&str> = config
                .layouts
                .iter()
                .flat_map(|layouts| layouts.keys().map(String::as_str))
                .collect();
            available.sort_unstable();
            bail!(
                "Layout '{}' not found. Available layouts: {}",
                name,
                [DEFAULT_LAYOUT]
                    .into_iter()
                    .chain(available)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        _ => match (&config.panes, &config.windows) {
            (Some(panes), _) => Ok(panes.clone()),
            (None, Some(_)) => bail!("'windows' layouts are not supported, only 'panes'"),
            (None, None) => bail!("No panes are configured"),
        },
    }
}

/// The layout names `--list` prints: the top-level panes first.
fn layout_names(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = config
        .layouts
        .iter()
        .flat_map(|layouts| layouts.keys().cloned())
        .filter(|name| name != DEFAULT_LAYOUT)
        .collect();
    names.sort_unstable();
    names.insert(0, DEFAULT_LAYOUT.to_string());

    names
}

fn label(identity: &Identity, index: usize) -> String {
    match identity {
        Identity::Agent => "agent".to_string(),
        Identity::Named(name) => name.clone(),
        Identity::Unknown => format!("pane {} (no name)", index + 1),
    }
}

/// Every pane name the config gives: the top-level panes and each layout's.
fn known_names(config: &Config) -> HashSet<String> {
    config
        .panes
        .iter()
        .flatten()
        .chain(
            config
                .layouts
                .iter()
                .flat_map(|layouts| layouts.values())
                .flat_map(|layout| &layout.panes),
        )
        .filter_map(|pane| pane.name.clone())
        .collect()
}

/// The panes to close: ones named after a pane of some layout (`known`) that
/// no pane of the chosen layout matched (`used`). An agent pane never is.
pub(crate) fn closable<'a>(
    window: &'a [WindowPane],
    used: &[&str],
    known: &HashSet<String>,
    agents: &[String],
) -> Vec<&'a WindowPane> {
    window
        .iter()
        .filter(|pane| !used.contains(&pane.pane_id.as_str()) && !agents.contains(&pane.pane_id))
        .filter(|pane| {
            pane.name.as_ref().is_some_and(|name| known.contains(name))
                || known.contains(&pane.title)
        })
        .collect()
}

fn pane_label(pane: &WindowPane) -> String {
    pane.name.clone().unwrap_or_else(|| pane.title.clone())
}

fn extent(pane: &WindowPane, direction: &SplitDirection) -> u16 {
    match direction {
        SplitDirection::Horizontal => pane.width,
        SplitDirection::Vertical | SplitDirection::Stacked => pane.height,
    }
}

pub fn run(
    layout: Option<&str>,
    list: bool,
    add_only: bool,
    dry_run: bool,
    target: Option<&str>,
) -> Result<()> {
    let mux = create_backend(detect_backend());
    let target = target
        .map(str::to_string)
        .or_else(|| mux.current_pane_id())
        .or_else(|| mux.active_pane_id())
        .ok_or_else(|| anyhow!("Not inside {}: pass -t <pane>", mux.name()))?;
    let window: Vec<WindowPane> = mux
        .window_panes(&target)?
        .into_iter()
        .filter(|pane| pane.role.as_deref() != Some(SIDEBAR_ROLE))
        .collect();
    let anchor = window
        .iter()
        .find(|pane| pane.pane_id == target)
        .or_else(|| window.first())
        .ok_or_else(|| anyhow!("The window of {target} has no panes but the sidebar"))?;
    let start_dir = anchor.current_path.clone();

    let (config, location) = Config::load_with_location_from(&start_dir, None)?;
    if list {
        for name in layout_names(&config) {
            println!("{name}");
        }
        return Ok(());
    }

    let panes = layout_panes(&config, layout)?;
    config::validate_panes_config(&panes)?;
    let worktree_root = crate::git::worktree_root_if_present(&start_dir)?
        .ok_or_else(|| anyhow!("{} is not in a git worktree", start_dir.display()))?;
    let working_dir = location
        .map(|location| worktree_root.join(location.rel_dir))
        .filter(|dir| dir.is_dir())
        .unwrap_or(worktree_root);

    let shell = mux.get_default_shell()?;
    let effective = effective_panes(&panes);
    let identities: Vec<Identity> = effective
        .iter()
        .map(|(pane, _)| {
            let is_agent = util::resolve_pane_command_with_config(
                pane.command.as_deref(),
                true,
                None,
                &working_dir,
                &config,
                None,
                &shell,
            )
            .is_some_and(|resolved| resolved.selected_agent.is_some());
            match (is_agent, &pane.name) {
                (true, _) => Identity::Agent,
                (false, Some(name)) => Identity::Named(name.clone()),
                (false, None) => Identity::Unknown,
            }
        })
        .collect();

    let agent_panes = agent_pane_ids(mux.as_ref(), &window)?;
    let mut used: Vec<&str> = Vec::new();
    let mut opened: Vec<Option<String>> = identities
        .iter()
        .map(|identity| {
            let found = window.iter().find(|pane| {
                !used.contains(&pane.pane_id.as_str())
                    && match identity {
                        Identity::Agent => agent_panes.contains(&pane.pane_id),
                        Identity::Named(name) => {
                            pane.name.as_ref() == Some(name) || &pane.title == name
                        }
                        Identity::Unknown => false,
                    }
            })?;
            used.push(&found.pane_id);
            Some(found.pane_id.clone())
        })
        .collect();

    let mut to_close: Vec<&WindowPane> = if add_only {
        Vec::new()
    } else {
        closable(&window, &used, &known_names(&config), &agent_panes)
    };
    // Closing every pane would close the window.
    let keeps_window = to_close.len() < window.len();
    let mut kept = Vec::new();
    if !keeps_window {
        kept = to_close.iter().map(|pane| pane_label(pane)).collect();
        to_close.clear();
    }

    let targets: Vec<Option<usize>> = effective.iter().map(|(_, target)| *target).collect();
    let present: Vec<bool> = opened.iter().map(Option::is_some).collect();
    let creatable: Vec<bool> = identities
        .iter()
        .map(|identity| matches!(identity, Identity::Named(_)))
        .collect();
    let (steps, unplaced) = plan(&targets, &present, &creatable);
    let step_pane = |step: &Step| match *step {
        Step::After { pane, .. } | Step::Before { pane, .. } => pane,
    };

    let mut added = Vec::new();
    if dry_run {
        added = steps
            .iter()
            .map(|step| label(&identities[step_pane(step)], step_pane(step)))
            .collect();
    } else if !steps.is_empty()
        && config.sandbox.is_enabled()
        && matches!(config.sandbox.target(), SandboxTarget::All)
    {
        bail!("workmux panes does not open sandboxed panes");
    }

    let closed: Vec<String> = to_close.iter().map(|pane| pane_label(pane)).collect();
    // Closed first: the panes that open size themselves on what is left.
    for pane in to_close.iter().filter(|_| !dry_run) {
        mux.kill_pane(&pane.pane_id)
            .with_context(|| format!("Failed to close {}", pane_label(pane)))?;
    }
    let is_closed = |id: &str| to_close.iter().any(|pane| pane.pane_id == id);
    // A closed active pane hands the focus to the layout's focus pane.
    let active = window
        .iter()
        .find(|pane| pane.is_active)
        .map(|pane| pane.pane_id.clone())
        .filter(|id| !is_closed(id))
        .or_else(|| {
            effective
                .iter()
                .zip(&opened)
                .find(|((pane, _), id)| pane.focus && id.is_some())
                .and_then(|(_, id)| id.clone())
        });
    for step in steps.iter().filter(|_| !dry_run) {
        let (pane, new_id) = open_pane(
            mux.as_ref(),
            *step,
            &effective,
            &opened,
            &working_dir,
            &config,
            &shell,
        )
        .with_context(|| "Failed to open a pane")?;
        opened[pane] = Some(new_id);
        added.push(label(&identities[pane], pane));
    }
    if !dry_run
        && !(steps.is_empty() && to_close.is_empty())
        && let Some(active) = active
    {
        mux.select_pane(&active)?;
    }

    let mut lines = Vec::new();
    if !added.is_empty() {
        let verb = if dry_run { "would open" } else { "opened" };
        lines.push(format!("{verb}: {}", added.join(", ")));
    }
    if !closed.is_empty() {
        let verb = if dry_run { "would close" } else { "closed" };
        lines.push(format!("{verb}: {}", closed.join(", ")));
    }
    if !kept.is_empty() {
        lines.push(format!(
            "not closed: {} (it would close the window)",
            kept.join(", ")
        ));
    }
    let missing_agents = identities
        .iter()
        .enumerate()
        .filter(|(k, identity)| **identity == Identity::Agent && opened[*k].is_none())
        .count();
    if missing_agents > 0 {
        lines.push("not opened: agent (start it yourself)".to_string());
    }
    let unknown: Vec<String> = identities
        .iter()
        .enumerate()
        .filter(|(_, identity)| **identity == Identity::Unknown)
        .map(|(k, identity)| label(identity, k))
        .collect();
    if !unknown.is_empty() {
        lines.push(format!("skipped: {} (give it a name)", unknown.join(", ")));
    }
    if !unplaced.is_empty() {
        let names: Vec<String> = unplaced.iter().map(|&k| label(&identities[k], k)).collect();
        lines.push(format!(
            "not opened: {} (no pane of the layout to split it from)",
            names.join(", ")
        ));
    }
    if lines.is_empty() {
        lines.push("all panes are open".to_string());
    }
    println!("{}", lines.join("; "));

    Ok(())
}

/// The window's panes that run a tracked agent.
fn agent_pane_ids(mux: &dyn Multiplexer, window: &[WindowPane]) -> Result<Vec<String>> {
    let backend = mux.name();
    let instance = mux.instance_id();

    Ok(StateStore::new()?
        .list_all_agents()?
        .into_iter()
        .filter(|agent| agent.pane_key.backend == backend && agent.pane_key.instance == instance)
        .map(|agent| agent.pane_key.pane_id)
        .filter(|id| window.iter().any(|pane| &pane.pane_id == id))
        .collect())
}

/// Opens one pane as `setup_panes` does (a shell, then its command), and
/// names it. Returns its layout index and pane id.
fn open_pane(
    mux: &dyn Multiplexer,
    step: Step,
    effective: &[(&PaneConfig, Option<usize>)],
    opened: &[Option<String>],
    working_dir: &Path,
    config: &Config,
    shell: &str,
) -> Result<(usize, String)> {
    let pane_index = match step {
        Step::After { pane, .. } | Step::Before { pane, .. } => pane,
    };
    let pane = effective[pane_index].0;
    let resolved = util::resolve_pane_command_with_config(
        pane.command.as_deref(),
        true,
        None,
        working_dir,
        config,
        None,
        shell,
    );
    let handshake = match resolved {
        Some(_) => Some(mux.create_handshake()?),
        None => None,
    };
    // A pane with no command is a plain shell, as in `setup_panes`.
    let script = handshake.as_ref().map(|h| h.script_content(shell));
    let id_of = |k: usize| {
        opened[k]
            .clone()
            .ok_or_else(|| anyhow!("pane {} is not open", k + 1))
    };

    let new_id = match step {
        Step::After { target, .. } => mux.split_pane(
            &id_of(target)?,
            pane.split.as_ref().unwrap_or(&SplitDirection::Vertical),
            working_dir,
            pane.size,
            pane.percentage,
            script.as_deref(),
        )?,
        Step::Before { child, .. } => {
            let child_id = id_of(child)?;
            let child_config = effective[child].0;
            let direction = child_config
                .split
                .as_ref()
                .unwrap_or(&SplitDirection::Vertical);
            let child_extent = mux
                .window_panes(&child_id)?
                .iter()
                .find(|p| p.pane_id == child_id)
                .map(|p| extent(p, direction));
            let (size, percentage) = before_size(child_config, child_extent);
            mux.split_pane_before(
                &child_id,
                direction,
                working_dir,
                size,
                percentage,
                script.as_deref(),
            )?
        }
    };

    if let (Some(handshake), Some(resolved)) = (handshake, resolved) {
        handshake.wait()?;
        let _ = mux.clear_pane(&new_id);
        mux.send_keys(&new_id, &resolved.render_command())?;
    }
    if let Some(name) = pane.name.as_deref() {
        mux.set_pane_name(&new_id, name)
            .with_context(|| format!("Failed to set pane name '{name}'"))?;
    }

    Ok((pane_index, new_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(
        split: Option<SplitDirection>,
        size: Option<u16>,
        percentage: Option<u8>,
    ) -> PaneConfig {
        PaneConfig {
            split,
            size,
            percentage,
            ..PaneConfig::default()
        }
    }

    fn window_pane(id: &str, name: Option<&str>, title: &str) -> WindowPane {
        WindowPane {
            pane_id: id.to_string(),
            role: None,
            name: name.map(str::to_string),
            title: title.to_string(),
            current_path: std::path::PathBuf::from("/repo"),
            width: 100,
            height: 10,
            is_active: false,
        }
    }

    #[test]
    fn only_unmatched_panes_some_layout_names_are_closed() {
        let window = vec![
            window_pane("%1", None, "✳ Claude"),
            window_pane("%2", Some("[server] dev"), "[server] dev"),
            // Opened before panes had names: told by its title.
            window_pane("%3", None, "[code]"),
            window_pane("%4", None, "zsh"),
            window_pane("%5", Some("[code]"), "[code]"),
        ];
        let known: HashSet<String> = ["[server] dev", "[code]"].map(String::from).into();
        let agents = vec!["%1".to_string()];
        // The default layout matched the agent and one [code].
        let close: Vec<&str> = closable(&window, &["%1", "%5"], &known, &agents)
            .iter()
            .map(|pane| pane.pane_id.as_str())
            .collect();
        // The server and the second [code] go; the agent and the shell stay.
        assert_eq!(close, vec!["%2", "%3"]);
    }

    #[test]
    fn an_agent_pane_is_never_closed_even_if_named() {
        let window = vec![window_pane("%1", Some("[code]"), "[code]")];
        let known: HashSet<String> = ["[code]"].map(String::from).into();
        assert!(closable(&window, &[], &known, &["%1".to_string()]).is_empty());
    }

    #[test]
    fn a_missing_last_pane_splits_from_its_target() {
        // agent, [code] below it; [code] is gone.
        let (steps, unplaced) = plan(&[None, Some(0)], &[true, false], &[false, true]);
        assert_eq!(steps, vec![Step::After { pane: 1, target: 0 }]);
        assert!(unplaced.is_empty());
    }

    #[test]
    fn a_missing_first_pane_goes_before_the_pane_split_from_it() {
        // dev: server, agent split from it, [code] from the agent; server is gone.
        let (steps, unplaced) = plan(
            &[None, Some(0), Some(1)],
            &[false, true, true],
            &[true, false, true],
        );
        assert_eq!(steps, vec![Step::Before { pane: 0, child: 1 }]);
        assert!(unplaced.is_empty());
    }

    #[test]
    fn a_pane_splits_from_one_this_run_opened() {
        // agent alone: [code] after it, then a pane split from [code].
        let (steps, _) = plan(
            &[None, Some(0), Some(1)],
            &[true, false, false],
            &[false, true, true],
        );
        assert_eq!(
            steps,
            vec![
                Step::After { pane: 1, target: 0 },
                Step::After { pane: 2, target: 1 }
            ]
        );
    }

    #[test]
    fn without_the_agent_its_dependents_cannot_be_placed() {
        let (steps, unplaced) = plan(&[None, Some(0)], &[false, false], &[false, true]);
        assert!(steps.is_empty());
        assert_eq!(unplaced, vec![1]);
    }

    #[test]
    fn nothing_missing_plans_nothing() {
        let (steps, unplaced) = plan(&[None, Some(0)], &[true, true], &[false, true]);
        assert!(steps.is_empty());
        assert!(unplaced.is_empty());
    }

    #[test]
    fn before_takes_the_complement_or_the_rest_of_the_extent() {
        let by_percentage = pane(Some(SplitDirection::Vertical), None, Some(97));
        assert_eq!(before_size(&by_percentage, Some(50)), (None, Some(3)));
        let by_size = pane(Some(SplitDirection::Vertical), Some(1), None);
        assert_eq!(before_size(&by_size, Some(40)), (Some(38), None));
        assert_eq!(before_size(&by_size, Some(2)), (None, None));
        assert_eq!(before_size(&pane(None, None, None), Some(40)), (None, None));
    }

    #[test]
    fn effective_panes_skip_unsplit_ones_and_count_targets_among_the_rest() {
        let panes = vec![
            pane(None, None, None),
            pane(None, None, None),
            pane(Some(SplitDirection::Vertical), None, None),
            PaneConfig {
                target: Some(0),
                ..pane(Some(SplitDirection::Horizontal), None, None)
            },
            PaneConfig {
                target: Some(5),
                ..pane(Some(SplitDirection::Horizontal), None, None)
            },
        ];
        let targets: Vec<Option<usize>> = effective_panes(&panes).iter().map(|(_, t)| *t).collect();
        assert_eq!(targets, vec![None, Some(0), Some(0), None]);
    }
}
