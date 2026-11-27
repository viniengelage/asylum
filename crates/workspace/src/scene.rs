use anyhow::{Context as _, Result};
use collections::HashSet;
use gpui::{
    Action, AnyElement, App, AppContext as _, Axis, Context, Div, Entity, Focusable as _,
    InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString, Styled as _,
    TaskExt as _, Window, actions, div, px,
};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{SceneAxis, SceneContent, SceneNodeContent, Settings as _};
use ui::{
    ButtonCommon as _, ButtonLike, ButtonStyle, Color, ContextMenu,
    ContextMenuEntry, FluentBuilder as _, Icon, IconName, IconPosition, IconSize, Label,
    LabelCommon as _, LabelSize, PopoverMenu, Toggleable as _, Tooltip, h_flex,
};
use theme::ActiveTheme as _;
use util::ResultExt as _;

use crate::{
    DockPosition, NewTerminal, Pane, Workspace, WorkspaceSettings,
    item::ItemHandle,
    pane::ContentKind,
    pane_group::{Member, PaneAxis},
    panel_item::PanelItemRegistry,
    persistence::WorkspaceDb,
};

/// Switches the window to a scene.
#[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = workspace)]
#[serde(deny_unknown_fields)]
pub struct ActivateScene {
    /// The `id` of an entry in the `scenes` setting.
    pub id: String,
}

/// Switches the window to the scene bound to this shortcut number (1–9).
#[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = workspace)]
pub struct ActivateSceneByShortcut(pub u8);

actions!(
    workspace,
    [
        /// Leaves the current scene and shows the docks again.
        ExitScene,
        /// Reapplies the active scene, dropping changes made to the layout since.
        ResetScene,
        /// Saves the current arrangement into the active scene.
        UpdateScene,
        /// Saves the current arrangement as a new scene.
        SaveLayoutAsScene,
    ]
);

/// A scene's slot tree after validation: every slot has content or is the principal,
/// every split has at least two children, and each content kind has one slot.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScenePlan {
    pub tree: SlotTree,
    pub slots: Vec<SlotSpec>,
    pub principal: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SlotTree {
    Slot(usize),
    Split {
        axis: Axis,
        children: Vec<(f32, SlotTree)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SlotSpec {
    pub kinds: Vec<ContentKind>,
}

impl ScenePlan {
    pub fn from_layout(layout: &SceneNodeContent) -> Option<Self> {
        Self::build(layout, false)
    }

    /// Like [`Self::from_layout`], but a slot left with no content still counts: comparing
    /// layouts has to see a split pane even though nothing routes to its new half.
    fn from_layout_keeping_empty_slots(layout: &SceneNodeContent) -> Option<Self> {
        Self::build(layout, true)
    }

    fn build(layout: &SceneNodeContent, keep_empty_slots: bool) -> Option<Self> {
        let mut slots = Vec::new();
        let mut principal = None;
        let mut seen = HashSet::default();
        let tree = build_slot_tree(
            layout,
            keep_empty_slots,
            &mut slots,
            &mut principal,
            &mut seen,
        )?;
        let principal = principal
            .or_else(|| slot_with_kind(&slots, &ContentKind::editor()))
            .or_else(|| slot_with_kind(&slots, &ContentKind::terminal()))
            .unwrap_or(0);
        Some(Self {
            tree,
            slots,
            principal,
        })
    }

    pub fn slot_for_kind(&self, kind: &ContentKind) -> Option<usize> {
        slot_with_kind(&self.slots, kind)
    }
}

fn slot_with_kind(slots: &[SlotSpec], kind: &ContentKind) -> Option<usize> {
    slots.iter().position(|slot| slot.kinds.contains(kind))
}

fn build_slot_tree(
    node: &SceneNodeContent,
    keep_empty_slots: bool,
    slots: &mut Vec<SlotSpec>,
    principal: &mut Option<usize>,
    seen: &mut HashSet<String>,
) -> Option<SlotTree> {
    if node.children.is_empty() {
        // A kind has exactly one destination, so a repeat in a later slot is dropped
        // instead of making routing depend on tree order.
        let kinds = node
            .kinds
            .iter()
            .filter(|kind| !kind.is_empty() && seen.insert(kind.to_string()))
            .map(|kind| ContentKind::new(kind.clone()))
            .collect::<Vec<_>>();
        if kinds.is_empty() && !node.principal && !keep_empty_slots {
            return None;
        }
        slots.push(SlotSpec { kinds });
        let index = slots.len() - 1;
        if node.principal && principal.is_none() {
            *principal = Some(index);
        }
        return Some(SlotTree::Slot(index));
    }

    let mut children = Vec::new();
    for child in &node.children {
        if let Some(tree) = build_slot_tree(child, keep_empty_slots, slots, principal, seen) {
            let flex = child
                .flex
                .filter(|flex| flex.is_finite() && *flex > 0.)
                .unwrap_or(1.);
            children.push((flex, tree));
        }
    }

    match children.len() {
        0 => None,
        1 => children.pop().map(|(_, tree)| tree),
        _ => Some(SlotTree::Split {
            axis: match node.axis.unwrap_or_default() {
                SceneAxis::Horizontal => Axis::Horizontal,
                SceneAxis::Vertical => Axis::Vertical,
            },
            children,
        }),
    }
}

/// Scales flexes so they sum to their count, which is what [`PaneAxis`] requires
/// (and silently resets to equal widths when it does not hold).
fn normalized_flexes(flexes: &[f32]) -> Vec<f32> {
    let count = flexes.len() as f32;
    let total: f32 = flexes.iter().sum();
    if total <= 0. {
        return vec![1.; flexes.len()];
    }
    let mut normalized = flexes
        .iter()
        .map(|flex| flex * count / total)
        .collect::<Vec<_>>();
    let rest: f32 = normalized.iter().skip(1).sum();
    if let Some(first) = normalized.first_mut() {
        *first = count - rest;
    }
    normalized
}

fn build_member(tree: &SlotTree, panes: &[Entity<Pane>]) -> Option<Member> {
    match tree {
        SlotTree::Slot(index) => panes.get(*index).cloned().map(Member::Pane),
        SlotTree::Split { axis, children } => {
            let mut members = Vec::new();
            let mut flexes = Vec::new();
            for (flex, child) in children {
                if let Some(member) = build_member(child, panes) {
                    members.push(member);
                    flexes.push(*flex);
                }
            }
            match members.len() {
                0 => None,
                1 => members.pop(),
                _ => Some(Member::Axis(PaneAxis::load(
                    *axis,
                    members,
                    Some(normalized_flexes(&flexes)),
                ))),
            }
        }
    }
}

pub fn scenes(cx: &App) -> &[SceneContent] {
    &WorkspaceSettings::get_global(cx).scenes
}

pub fn scene_by_id<'a>(id: &str, cx: &'a App) -> Option<&'a SceneContent> {
    scenes(cx).iter().find(|scene| scene.id == id)
}

/// The scene's own name, or the interface-language name of a built-in scene.
pub fn scene_display_name(scene: &SceneContent) -> SharedString {
    if let Some(name) = scene.name.as_ref().filter(|name| !name.trim().is_empty()) {
        return SharedString::from(name.clone());
    }
    match scene.id.as_str() {
        "code" => i18n::t!("workspace.scene.builtin.code"),
        "mobile" => i18n::t!("workspace.scene.builtin.mobile"),
        "mobile_stacked" => i18n::t!("workspace.scene.builtin.mobile_stacked"),
        "review" => i18n::t!("workspace.scene.builtin.review"),
        id => SharedString::from(id.to_string()),
    }
}

/// How a content kind is called in the interface.
pub fn content_kind_display_name(kind: &str) -> SharedString {
    match kind {
        ContentKind::EDITOR => i18n::t!("workspace.scene.kind.editor"),
        ContentKind::TERMINAL => i18n::t!("workspace.scene.kind.terminal"),
        "Browser" => i18n::t!("workspace.scene.kind.browser"),
        "AgentPanel" => i18n::t!("workspace.scene.kind.agent"),
        "DevicesPanel" => i18n::t!("workspace.scene.kind.devices"),
        "Project Panel" => i18n::t!("workspace.scene.kind.files"),
        "GitPanel" => i18n::t!("workspace.scene.kind.git"),
        "Outline Panel" => i18n::t!("workspace.scene.kind.outline"),
        "DebugPanel" => i18n::t!("workspace.scene.kind.debug"),
        "ExecutionsPanel" => i18n::t!("workspace.scene.kind.runs"),
        "NotesPanel" => i18n::t!("workspace.scene.kind.notes"),
        "DatabasePanel" => i18n::t!("workspace.scene.kind.database"),
        "DockerPanel" => i18n::t!("workspace.scene.kind.docker"),
        "ApiPanel" => i18n::t!("workspace.scene.kind.api"),
        other => SharedString::from(other.trim_end_matches("Panel").trim().to_string()),
    }
}

/// One line listing what a scene shows, in tree order: "Devices · Terminal · Agent".
pub fn scene_summary(scene: &SceneContent) -> SharedString {
    let Some(plan) = ScenePlan::from_layout(&scene.layout) else {
        return SharedString::default();
    };
    let names = plan
        .slots
        .iter()
        .flat_map(|slot| slot.kinds.iter())
        .map(|kind| content_kind_display_name(kind.as_str()))
        .collect::<Vec<_>>();
    SharedString::from(names.join(" · "))
}

pub(crate) fn add_scene_action_listeners(div: Div, cx: &mut Context<Workspace>) -> Div {
    div.on_action(cx.listener(|workspace, action: &ActivateScene, window, cx| {
        workspace.activate_scene(&action.id, window, cx).log_err();
    }))
    .on_action(cx.listener(|workspace, action: &ActivateSceneByShortcut, window, cx| {
        let id = scenes(cx)
            .iter()
            .find(|scene| scene.shortcut == Some(action.0))
            .map(|scene| scene.id.clone());
        if let Some(id) = id {
            workspace.activate_scene(&id, window, cx).log_err();
        }
    }))
    .on_action(cx.listener(|workspace, _: &ExitScene, window, cx| {
        workspace.exit_scene(window, cx);
    }))
    .on_action(cx.listener(|workspace, _: &ResetScene, window, cx| {
        workspace.reset_active_scene(window, cx);
    }))
    .on_action(cx.listener(|workspace, _: &UpdateScene, _, cx| {
        workspace.update_active_scene(cx);
    }))
    .on_action(cx.listener(|workspace, _: &SaveLayoutAsScene, window, cx| {
        workspace.save_layout_as_scene(window, cx);
    }))
}

/// Whether two plans show the same slots, in the same places, at about the same sizes.
fn plans_match(a: &ScenePlan, b: &ScenePlan) -> bool {
    a.slots.len() == b.slots.len()
        && a.slots.iter().zip(&b.slots).all(|(a, b)| {
            a.kinds.len() == b.kinds.len() && a.kinds.iter().all(|kind| b.kinds.contains(kind))
        })
        && trees_match(&a.tree, &b.tree)
}

fn trees_match(a: &SlotTree, b: &SlotTree) -> bool {
    // Dragging a divider by a few pixels is not a different scene.
    const FLEX_TOLERANCE: f32 = 0.02;
    match (a, b) {
        (SlotTree::Slot(a), SlotTree::Slot(b)) => a == b,
        (
            SlotTree::Split {
                axis: a_axis,
                children: a_children,
            },
            SlotTree::Split {
                axis: b_axis,
                children: b_children,
            },
        ) => {
            if a_axis != b_axis || a_children.len() != b_children.len() {
                return false;
            }
            let a_total: f32 = a_children.iter().map(|(flex, _)| flex).sum();
            let b_total: f32 = b_children.iter().map(|(flex, _)| flex).sum();
            a_children
                .iter()
                .zip(b_children)
                .all(|((a_flex, a_tree), (b_flex, b_tree))| {
                    (a_flex / a_total - b_flex / b_total).abs() <= FLEX_TOLERANCE
                        && trees_match(a_tree, b_tree)
                })
        }
        _ => false,
    }
}

fn member_to_node(member: &Member, principal: Option<&Entity<Pane>>, cx: &App) -> SceneNodeContent {
    match member {
        Member::Pane(pane) => SceneNodeContent {
            kinds: pane
                .read(cx)
                .accepts_kinds()
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
            principal: Some(pane) == principal,
            ..Default::default()
        },
        Member::Axis(axis) => {
            let flexes =
                PaneAxis::flexes_with_collapsed_shown(&axis.members, &axis.flexes.lock(), cx);
            let count = flexes.len().max(1) as f32;
            SceneNodeContent {
                axis: Some(match axis.axis {
                    Axis::Horizontal => SceneAxis::Horizontal,
                    Axis::Vertical => SceneAxis::Vertical,
                }),
                children: axis
                    .members
                    .iter()
                    .zip(flexes)
                    .map(|(member, flex)| {
                        let mut node = member_to_node(member, principal, cx);
                        node.flex = Some((flex / count * 1000.).round() / 1000.);
                        node
                    })
                    .collect(),
                ..Default::default()
            }
        }
    }
}

impl Workspace {
    /// Whether this window shows a scene. Outside a scene the docks are the
    /// presentation surface, as in upstream Zed.
    pub fn in_scene(&self) -> bool {
        self.active_scene.is_some()
    }

    pub fn active_scene_id(&self) -> Option<&SharedString> {
        self.active_scene.as_ref()
    }

    /// The slot receiving content the scene has no slot for.
    ///
    /// Derived rather than stored, so it survives a restart and follows the slots the
    /// user dragged content into.
    pub fn principal_slot(&self, cx: &App) -> Option<Entity<Pane>> {
        let scene_id = self.active_scene.as_ref()?;
        let declared = scene_by_id(scene_id, cx)
            .and_then(|scene| ScenePlan::from_layout(&scene.layout))
            .and_then(|plan| plan.slots.get(plan.principal).cloned());
        let panes = self.center.panes();
        if let Some(declared) = declared
            && let Some(pane) = panes.iter().find(|pane| {
                declared
                    .kinds
                    .iter()
                    .any(|kind| pane.read(cx).accepts_kind(kind))
            })
        {
            return Some((*pane).clone());
        }
        let editor = ContentKind::editor();
        panes
            .iter()
            .find(|pane| pane.read(cx).accepts_kind(&editor))
            .or_else(|| panes.iter().find(|pane| !pane.read(cx).hosts_panels))
            .or_else(|| panes.first())
            .map(|pane| (*pane).clone())
    }

    pub fn activate_scene(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let scene = scene_by_id(id, cx)
            .with_context(|| format!("no scene with id {id:?}"))?
            .clone();
        let plan = ScenePlan::from_layout(&scene.layout)
            .with_context(|| format!("scene {id:?} has no slots"))?;

        self.apply_scene_plan(&plan, window, cx);
        self.active_scene = Some(SharedString::from(scene.id));
        self.persist_active_scene(cx);
        cx.emit(crate::Event::SceneChanged);
        cx.notify();
        Ok(())
    }

    fn apply_scene_plan(&mut self, plan: &ScenePlan, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_scene.is_none() {
            self.docks_before_scene = Some(self.open_dock_positions(cx));
        }
        self.set_docks_suppressed(true, window, cx);
        self.reset_zoom_for_layout_change(cx);

        // A container panel (the terminal) keeps its items in panes of its own, which
        // nothing renders while a scene is shown; they join the pane tree instead.
        let mut loose_items = Vec::new();
        for panel in self.panels.clone() {
            loose_items.extend(panel.take_hosted_items(window, cx));
        }

        let old_panes = self
            .center
            .panes()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let slot_panes = self.assign_slot_panes(plan, &old_panes, window, cx);
        let editor = ContentKind::editor();
        for (index, (spec, pane)) in plan.slots.iter().zip(&slot_panes).enumerate() {
            let hosts_content = index == plan.principal || spec.kinds.contains(&editor);
            pane.update(cx, |pane, cx| {
                pane.set_accepts_kinds(spec.kinds.clone(), cx);
                pane.set_collapsed_flex(None);
                pane.hosts_panels = !hosts_content;
                pane.set_can_navigate(hosts_content, cx);
                // Closing the last terminal must not take the terminal slot with it.
                pane.set_close_pane_if_empty(false, cx);
            });
        }
        let Some(principal) = slot_panes.get(plan.principal).cloned() else {
            return;
        };

        let target_for = |item: &dyn ItemHandle, cx: &App| -> Option<Entity<Pane>> {
            let kind = item.content_kind(cx);
            if let Some(index) = kind.as_ref().and_then(|kind| plan.slot_for_kind(kind)) {
                return slot_panes.get(index).cloned();
            }
            // A panel the scene leaves out stays registered in `Workspace::panels`; only
            // its tab goes away.
            if PanelItemRegistry::is_panel_item(&item.to_any_view(), cx) {
                None
            } else {
                Some(principal.clone())
            }
        };

        let mut panes_to_drain = old_panes;
        for pane in &slot_panes {
            if !panes_to_drain.contains(pane) {
                panes_to_drain.push(pane.clone());
            }
        }
        for pane in &panes_to_drain {
            let items = pane
                .read(cx)
                .items()
                .map(|item| item.boxed_clone())
                .collect::<Vec<_>>();
            for item in items {
                match target_for(item.as_ref(), cx) {
                    Some(target) if &target == pane => {}
                    Some(target) => relocate_item(pane, &target, item, window, cx),
                    None => pane.update(cx, |pane, cx| {
                        pane.remove_item(item.item_id(), false, false, window, cx);
                    }),
                }
            }
        }
        for item in loose_items {
            let target = target_for(item.as_ref(), cx).unwrap_or_else(|| principal.clone());
            target.update(cx, |target, cx| {
                target.add_item(item, false, false, None, window, cx);
            });
        }

        // Panels created on first use (notes, JSON prettier) may not exist yet.
        for kind in plan.slots.iter().flat_map(|spec| spec.kinds.iter()) {
            if self.panel_for_persistent_name(kind.as_str()).is_none()
                && let Some(restore) = cx
                    .try_global::<crate::LazyPanels>()
                    .and_then(|lazy| lazy.0.get(kind.as_str()).copied())
            {
                restore(self, window, cx);
            }
        }

        for (spec, pane) in plan.slots.iter().zip(&slot_panes) {
            for kind in &spec.kinds {
                let Some(panel) = self.panel_for_persistent_name(kind.as_str()).cloned() else {
                    continue;
                };
                if panel.enabled(cx) && panel.hosted_content_kind(cx).is_none() {
                    self.open_panel_in_pane(&panel, pane, false, window, cx);
                }
            }
            show_first_declared_item(pane, &spec.kinds, window, cx);
        }

        let Some(root) = build_member(&plan.tree, &slot_panes) else {
            return;
        };
        self.apply_pane_layout(root, window, cx);
        self.set_active_pane(&principal, window, cx);
        window.focus(&principal.focus_handle(cx), cx);

        // A terminal slot that opens empty would be a blank box; the docks open a shell
        // in the same situation.
        let terminal = ContentKind::terminal();
        let empty_terminal_slot = plan
            .slots
            .iter()
            .zip(&slot_panes)
            .any(|(spec, pane)| spec.kinds.contains(&terminal) && pane.read(cx).items_len() == 0);
        if empty_terminal_slot {
            window.defer(cx, |window, cx| {
                window.dispatch_action(Box::new(NewTerminal::default()), cx);
            });
        }
    }

    /// Picks a pane for every slot, reusing panes that already show the slot's content
    /// so editors and terminals stay where they are and nothing is rebuilt.
    fn assign_slot_panes(
        &mut self,
        plan: &ScenePlan,
        old_panes: &[Entity<Pane>],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Entity<Pane>> {
        let mut assigned: Vec<Option<Entity<Pane>>> = vec![None; plan.slots.len()];
        let mut used = vec![false; old_panes.len()];

        let mut candidates = Vec::new();
        for (slot_index, spec) in plan.slots.iter().enumerate() {
            for (pane_index, pane) in old_panes.iter().enumerate() {
                let pane = pane.read(cx);
                let overlap = spec
                    .kinds
                    .iter()
                    .filter(|kind| pane.accepts_kind(kind))
                    .count();
                if overlap > 0 {
                    candidates.push((overlap, slot_index, pane_index));
                }
            }
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
        for (_, slot_index, pane_index) in candidates {
            if assigned[slot_index].is_none() && !used[pane_index] {
                assigned[slot_index] = old_panes.get(pane_index).cloned();
                used[pane_index] = true;
            }
        }

        // Coming from the docks the editor pane declares nothing; it becomes the editor
        // slot, or the principal when the scene has no editor.
        let editor_slot = plan
            .slot_for_kind(&ContentKind::editor())
            .unwrap_or(plan.principal);
        if assigned.get(editor_slot).is_some_and(Option::is_none) {
            let last_active = self
                .last_active_center_pane
                .as_ref()
                .and_then(|pane| pane.upgrade());
            let reusable = |pane: &Entity<Pane>| {
                let pane = pane.read(cx);
                !pane.hosts_panels && pane.accepts_kinds().is_empty()
            };
            let pick = old_panes
                .iter()
                .enumerate()
                .filter(|(index, pane)| !used[*index] && reusable(pane))
                .max_by_key(|(_, pane)| Some(*pane) == last_active.as_ref());
            if let Some((index, pane)) = pick {
                used[index] = true;
                assigned[editor_slot] = Some(pane.clone());
            }
        }

        let editor = ContentKind::editor();
        assigned
            .into_iter()
            .enumerate()
            .map(|(index, pane)| match pane {
                Some(pane) => pane,
                None => {
                    let hosts_content = index == plan.principal
                        || plan
                            .slots
                            .get(index)
                            .is_some_and(|spec| spec.kinds.contains(&editor));
                    if hosts_content {
                        self.add_pane(window, cx)
                    } else {
                        self.add_panel_pane(window, cx)
                    }
                }
            })
            .collect()
    }

    /// Leaves the scene: panels go back to their docks, terminals to the terminal
    /// panel, and everything else into a single editor pane.
    pub fn exit_scene(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_scene.is_none() {
            return;
        }
        self.reset_zoom_for_layout_change(cx);

        let panes = self
            .center
            .panes()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let destination = self
            .principal_slot(cx)
            .unwrap_or_else(|| self.add_pane(window, cx));
        destination.update(cx, |pane, cx| {
            pane.set_accepts_kinds(Vec::new(), cx);
            pane.set_collapsed_flex(None);
            pane.hosts_panels = false;
            pane.set_can_navigate(true, cx);
            pane.set_close_pane_if_empty(true, cx);
        });

        let terminal = ContentKind::terminal();
        let terminal_host = self
            .panels
            .iter()
            .find(|panel| panel.hosted_content_kind(cx).as_ref() == Some(&terminal))
            .cloned();

        for pane in &panes {
            let items = pane
                .read(cx)
                .items()
                .map(|item| item.boxed_clone())
                .collect::<Vec<_>>();
            for item in items {
                if PanelItemRegistry::is_panel_item(&item.to_any_view(), cx) {
                    pane.update(cx, |pane, cx| {
                        pane.remove_item(item.item_id(), false, false, window, cx);
                    });
                    continue;
                }
                if let Some(host) = terminal_host.as_ref()
                    && item.content_kind(cx).as_ref() == Some(&terminal)
                {
                    pane.update(cx, |pane, cx| {
                        pane.remove_item(item.item_id(), false, false, window, cx);
                    });
                    if let Some(rejected) = host.adopt_hosted_item(item, window, cx) {
                        destination.update(cx, |destination, cx| {
                            destination.add_item(rejected, false, false, None, window, cx);
                        });
                    }
                    continue;
                }
                if pane != &destination {
                    relocate_item(pane, &destination, item, window, cx);
                }
            }
        }

        self.active_scene = None;
        self.apply_pane_layout(Member::Pane(destination.clone()), window, cx);
        self.set_docks_suppressed(false, window, cx);
        let open_docks = self
            .docks_before_scene
            .take()
            .unwrap_or_else(|| vec![DockPosition::Left, DockPosition::Right]);
        for dock in self.all_docks() {
            let open = open_docks.contains(&dock.read(cx).position());
            dock.update(cx, |dock, cx| dock.set_open(open, window, cx));
        }
        window.focus(&destination.focus_handle(cx), cx);

        self.persist_active_scene(cx);
        cx.emit(crate::Event::SceneChanged);
        cx.notify();
    }

    /// Reads the window's scene before its pane tree is restored, so restored panel
    /// tabs and late-registering panels are placed as a scene expects.
    pub(crate) fn restore_active_scene(
        &mut self,
        workspace_id: crate::WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = WorkspaceDb::global(cx)
            .active_scene(workspace_id)
            .log_err()
            .flatten()
            .flatten();
        if let Some(id) = saved {
            self.active_scene = Some(SharedString::from(id));
            self.set_docks_suppressed(true, window, cx);
        }
    }

    /// Called once the pane tree is restored: a scene that was deleted from the
    /// settings since the window was last open has nothing to restore into.
    pub(crate) fn finish_scene_restoration(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_scene.clone() else {
            return;
        };
        if scene_by_id(&id, cx).is_none() {
            self.exit_scene(window, cx);
            return;
        }
        self.set_docks_suppressed(true, window, cx);
        for pane in self.panes.clone() {
            if !pane.read(cx).accepts_kinds().is_empty() {
                pane.update(cx, |pane, cx| pane.set_close_pane_if_empty(false, cx));
            }
        }
    }

    fn persist_active_scene(&self, cx: &mut Context<Self>) {
        let Some(database_id) = self.database_id() else {
            return;
        };
        let db = WorkspaceDb::global(cx);
        let scene = self.active_scene.as_ref().map(|id| id.to_string());
        cx.background_spawn(async move { db.set_active_scene(database_id, scene).await })
            .detach_and_log_err(cx);
    }

    fn open_dock_positions(&self, cx: &App) -> Vec<DockPosition> {
        self.all_docks()
            .iter()
            .filter(|dock| dock.read(cx).is_open())
            .map(|dock| dock.read(cx).position())
            .collect()
    }

    fn set_docks_suppressed(&mut self, suppressed: bool, window: &mut Window, cx: &mut Context<Self>) {
        for dock in self.all_docks() {
            dock.update(cx, |dock, cx| dock.set_suppressed(suppressed, window, cx));
        }
    }

    fn reset_zoom_for_layout_change(&mut self, cx: &mut Context<Self>) {
        for pane in self.panes.clone() {
            pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
        }
        self.maximized_pane = None;
        if self.zoomed.take().is_some() {
            self.zoomed_position = None;
            cx.emit(crate::Event::ZoomChanged);
        }
    }

    /// Moves an item a container panel just received into the scene's pane tree.
    ///
    /// Tasks, the agent and the panel's own restore add terminals to the terminal
    /// panel's panes; in a scene nothing renders those, so the terminal would run
    /// invisibly.
    pub fn adopt_into_scene(
        &mut self,
        source: &Entity<Pane>,
        item: Box<dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.in_scene() {
            return;
        }
        let target = item
            .content_kind(cx)
            .and_then(|kind| self.pane_for_kind(&kind, cx))
            .or_else(|| self.principal_slot(cx));
        let Some(target) = target else {
            return;
        };
        let had_focus = item.item_focus_handle(cx).contains_focused(window, cx);
        source.update(cx, |source, cx| {
            source.remove_item(item.item_id(), false, false, window, cx);
        });
        if target.read(cx).is_collapsed() {
            self.center.set_pane_collapsed(&target, false, cx);
        }
        target.update(cx, |target, cx| {
            target.add_item(item, true, had_focus, None, window, cx);
        });
        cx.notify();
    }
}

impl Workspace {
    /// The window's arrangement written as a scene layout. Hidden slots are written at
    /// the size they come back at.
    pub fn current_scene_layout(&self, cx: &App) -> SceneNodeContent {
        let principal = self.principal_slot(cx);
        let mut root = member_to_node(&self.center.root, principal.as_ref(), cx);
        root.flex = None;
        root
    }

    /// Whether the user rearranged the window since the scene was applied.
    pub fn active_scene_is_modified(&self, cx: &App) -> bool {
        let Some(scene) = self
            .active_scene
            .as_ref()
            .and_then(|id| scene_by_id(id, cx))
        else {
            return false;
        };
        let saved = ScenePlan::from_layout_keeping_empty_slots(&scene.layout);
        let current = ScenePlan::from_layout_keeping_empty_slots(&self.current_scene_layout(cx));
        match (saved, current) {
            (Some(saved), Some(current)) => !plans_match(&saved, &current),
            _ => true,
        }
    }

    /// Drops the changes made to the layout since the active scene was applied.
    pub fn reset_active_scene(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_scene.clone() {
            self.activate_scene(&id, window, cx).log_err();
        }
    }

    /// Writes the current arrangement into the active scene, in the settings file.
    pub fn update_active_scene(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active_scene.clone() else {
            return;
        };
        let layout = self.current_scene_layout(cx);
        let mut scenes = scenes(cx).to_vec();
        let Some(scene) = scenes.iter_mut().find(|scene| scene.id == id.as_ref()) else {
            return;
        };
        scene.layout = layout;
        self.write_scenes(scenes, cx);
    }

    /// Saves the current arrangement as a new scene and makes it the window's scene.
    pub fn save_layout_as_scene(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.in_scene() {
            // From the docks there is no slot tree to save yet; the closest scene is
            // the one that shows the same panels, so start from the default.
            if let Some(first) = scenes(cx).first().map(|scene| scene.id.clone()) {
                self.activate_scene(&first, window, cx).log_err();
            }
        }
        let mut scenes = scenes(cx).to_vec();
        let number = (1..)
            .find(|number| {
                !scenes
                    .iter()
                    .any(|scene| scene.id == format!("scene_{number}"))
            })
            .unwrap_or(1);
        let shortcut = (1..=9u8).find(|shortcut| {
            !scenes
                .iter()
                .any(|scene| scene.shortcut == Some(*shortcut))
        });
        let id = format!("scene_{number}");
        scenes.push(SceneContent {
            id: id.clone(),
            name: Some(i18n::t!("workspace.scene.new_name", number = number).to_string()),
            shortcut,
            layout: self.current_scene_layout(cx),
        });
        self.write_scenes(scenes, cx);
        // The layout on screen is the scene, so nothing has to be reapplied.
        self.active_scene = Some(SharedString::from(id));
        self.persist_active_scene(cx);
        cx.emit(crate::Event::SceneChanged);
        cx.notify();
    }

    fn write_scenes(&self, scenes: Vec<SceneContent>, cx: &mut Context<Self>) {
        let fs = self.project().read(cx).fs().clone();
        settings::update_settings_file(fs, cx, move |settings, _| {
            settings.workspace.scenes = Some(scenes);
        });
    }
}

/// The status bar control that shows the window's scene and switches between scenes.
pub(crate) fn render_scene_menu(
    workspace: &Workspace,
    workspace_handle: gpui::WeakEntity<Workspace>,
    cx: &App,
) -> AnyElement {
    let active = workspace.active_scene.clone();
    let active_scene = active.as_ref().and_then(|id| scene_by_id(id, cx));
    let modified = workspace.active_scene_is_modified(cx);
    let label = active_scene.map(scene_display_name);
    let tooltip = i18n::t!("workspace.scene.title");

    let trigger = ButtonLike::new("status-bar-scene")
        .style(ButtonStyle::Subtle)
        .toggle_state(active_scene.is_some())
        .height(crate::status_bar::status_bar_item_height(cx).into())
        .child(
            h_flex()
                .px_1()
                .gap_1()
                .child(
                    Icon::new(IconName::SplitAlt)
                        .size(IconSize::Small)
                        .color(if active_scene.is_some() {
                            Color::Accent
                        } else {
                            Color::Muted
                        }),
                )
                .when_some(label, |this, label| {
                    this.child(Label::new(label).size(LabelSize::Small))
                })
                .when(modified, |this| {
                    this.child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(cx.theme().status().warning),
                    )
                })
                .when(active_scene.is_some(), |this| {
                    this.child(
                        Icon::new(IconName::ChevronUpDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        )
        .tooltip(Tooltip::text(tooltip.clone()))
        .aria_label(tooltip);

    let entries = scenes(cx)
        .iter()
        .map(|scene| {
            (
                SharedString::from(scene.id.clone()),
                scene_display_name(scene),
                scene_summary(scene),
                scene.shortcut,
            )
        })
        .collect::<Vec<_>>();
    let active_name = active_scene.map(scene_display_name);

    PopoverMenu::new("status-bar-scene-menu")
        .anchor(gpui::Anchor::BottomLeft)
        .trigger(trigger)
        .menu(move |window, cx| {
            let entries = entries.clone();
            let active = active.clone();
            let active_name = active_name.clone();
            let workspace_handle = workspace_handle.clone();
            Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                let mut menu = menu.sectioned().header(i18n::t!("workspace.scene.section"));
                for (id, name, summary, shortcut) in &entries {
                    let is_active = active.as_ref() == Some(id);
                    let workspace_handle = workspace_handle.clone();
                    let id = id.clone();
                    let mut entry = ContextMenuEntry::new(name.clone())
                        .description(summary.clone())
                        .toggleable(IconPosition::End, is_active)
                        .handler(move |window, cx| {
                            workspace_handle
                                .update(cx, |workspace, cx| {
                                    workspace.activate_scene(&id, window, cx).log_err();
                                })
                                .log_err();
                        });
                    if let Some(shortcut) = shortcut {
                        entry = entry.action(Box::new(ActivateSceneByShortcut(*shortcut)));
                    }
                    menu = menu.item(entry);
                }
                menu = menu.item(
                    ContextMenuEntry::new(i18n::t!("workspace.scene.free"))
                        .description(i18n::t!("workspace.scene.free_description"))
                        .toggleable(IconPosition::End, active.is_none())
                        .action(Box::new(ExitScene))
                        .handler(on_workspace(&workspace_handle, |workspace, window, cx| {
                            workspace.exit_scene(window, cx)
                        })),
                );
                menu = menu.separator();
                if let Some(name) = active_name.clone() {
                    if modified {
                        menu = menu
                            .item(
                                ContextMenuEntry::new(i18n::t!(
                                    "workspace.scene.update",
                                    name = name
                                ))
                                .icon(IconName::Check)
                                .action(Box::new(UpdateScene))
                                .handler(on_workspace(
                                    &workspace_handle,
                                    |workspace, _, cx| workspace.update_active_scene(cx),
                                )),
                            )
                            .item(
                                ContextMenuEntry::new(i18n::t!("workspace.scene.reset"))
                                    .icon(IconName::Undo)
                                    .action(Box::new(ResetScene))
                                    .handler(on_workspace(
                                        &workspace_handle,
                                        |workspace, window, cx| {
                                            workspace.reset_active_scene(window, cx)
                                        },
                                    )),
                            );
                    }
                }
                menu.item(
                    ContextMenuEntry::new(i18n::t!("workspace.scene.save_as_new"))
                        .icon(IconName::Plus)
                        .action(Box::new(SaveLayoutAsScene))
                        .handler(on_workspace(&workspace_handle, |workspace, window, cx| {
                            workspace.save_layout_as_scene(window, cx)
                        })),
                )
            }))
        })
        .into_any_element()
}

/// A menu entry's click handler. An entry's `action` only labels its keybinding; the
/// click itself runs the handler, which is a no-op unless one is set.
fn on_workspace(
    workspace: &gpui::WeakEntity<Workspace>,
    run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let workspace = workspace.clone();
    move |window, cx| {
        workspace
            .update(cx, |workspace, cx| run(workspace, window, cx))
            .log_err();
    }
}

/// Moves an item between panes without the drag semantics of [`crate::move_item`]:
/// scene changes declare slot kinds themselves, so nothing should follow the item.
fn relocate_item(
    source: &Entity<Pane>,
    destination: &Entity<Pane>,
    item: Box<dyn ItemHandle>,
    window: &mut Window,
    cx: &mut App,
) {
    source.update(cx, |source, cx| {
        source.remove_item(item.item_id(), false, false, window, cx);
    });
    destination.update(cx, |destination, cx| {
        destination.add_item(item, false, false, None, window, cx);
    });
}

fn show_first_declared_item(
    pane: &Entity<Pane>,
    kinds: &[ContentKind],
    window: &mut Window,
    cx: &mut App,
) {
    let index = kinds.iter().find_map(|kind| {
        pane.read(cx)
            .items()
            .position(|item| item.content_kind(cx).as_ref() == Some(kind))
    });
    let index = index.or_else(|| (pane.read(cx).items_len() > 0).then_some(0));
    if let Some(index) = index {
        pane.update(cx, |pane, cx| pane.activate_item(index, false, false, window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(flex: f32, kinds: &[&str], principal: bool) -> SceneNodeContent {
        SceneNodeContent {
            flex: Some(flex),
            kinds: kinds.iter().map(|kind| kind.to_string()).collect(),
            principal,
            ..Default::default()
        }
    }

    fn split(flex: f32, axis: SceneAxis, children: Vec<SceneNodeContent>) -> SceneNodeContent {
        SceneNodeContent {
            flex: Some(flex),
            axis: Some(axis),
            children,
            ..Default::default()
        }
    }

    #[test]
    fn plan_keeps_rows_inside_columns() {
        let layout = split(
            1.,
            SceneAxis::Horizontal,
            vec![
                slot(0.25, &["DevicesPanel"], false),
                split(
                    0.75,
                    SceneAxis::Vertical,
                    vec![
                        slot(0.62, &["AgentPanel"], false),
                        slot(0.38, &["Terminal"], true),
                    ],
                ),
            ],
        );
        let plan = ScenePlan::from_layout(&layout).expect("plan");
        assert_eq!(plan.slots.len(), 3);
        assert_eq!(plan.principal, 2);
        let SlotTree::Split { axis, children } = &plan.tree else {
            panic!("root should split");
        };
        assert_eq!(*axis, Axis::Horizontal);
        assert!(matches!(
            children.get(1),
            Some((_, SlotTree::Split { axis: Axis::Vertical, .. }))
        ));
    }

    #[test]
    fn plan_drops_empty_slots_and_duplicate_kinds() {
        let layout = split(
            1.,
            SceneAxis::Horizontal,
            vec![
                slot(1., &["Editor"], false),
                slot(1., &[], false),
                slot(1., &["Editor", "Terminal"], false),
            ],
        );
        let plan = ScenePlan::from_layout(&layout).expect("plan");
        assert_eq!(plan.slots.len(), 2);
        assert_eq!(plan.slots[1].kinds, vec![ContentKind::terminal()]);
        // Without an explicit principal the editor slot receives everything else.
        assert_eq!(plan.principal, 0);
    }

    #[test]
    fn single_child_splits_collapse() {
        let layout = split(
            1.,
            SceneAxis::Horizontal,
            vec![split(1., SceneAxis::Vertical, vec![slot(1., &["Editor"], true)])],
        );
        let plan = ScenePlan::from_layout(&layout).expect("plan");
        assert_eq!(plan.tree, SlotTree::Slot(0));
    }

    #[test]
    fn flexes_are_scaled_to_their_count() {
        let flexes = normalized_flexes(&[0.25, 0.49, 0.26]);
        let total: f32 = flexes.iter().sum();
        assert!((total - 3.).abs() < 0.0001);
        assert!((flexes[1] - 1.47).abs() < 0.001);
    }

    #[test]
    fn default_scenes_are_valid() {
        let defaults = settings::default_settings();
        let content: serde_json::Value =
            settings::parse_json_with_comments(&defaults).expect("default settings parse");
        let scenes: Vec<SceneContent> =
            serde_json::from_value(content["scenes"].clone()).expect("scenes parse");
        assert!(!scenes.is_empty());
        for scene in &scenes {
            assert!(
                ScenePlan::from_layout(&scene.layout).is_some(),
                "scene {} has no slots",
                scene.id
            );
        }
    }
}
