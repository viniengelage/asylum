use std::{any::TypeId, sync::Arc};

use anyhow::{Result, anyhow};
use collections::HashMap;
use gpui::{
    AnyElement, AnyEntity, AnyView, App, AppContext as _, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Global, IntoElement, Render, SharedString, StyleRefinement,
    Styled as _, Subscription, Task, WeakEntity, Window,
};
use project::Project;
use ui::{Icon, Label, prelude::*};

use crate::{
    ToggleZoom, Workspace, WorkspaceId,
    dock::{Panel, PanelEvent, PanelHandle},
    item::{Item, ItemEvent, ItemHandle, SerializableItem, TabContentParams},
    persistence::model::ItemId,
};

type PanelItemBuilder =
    fn(&Arc<dyn PanelHandle>, &mut Window, &mut App) -> Option<Box<dyn ItemHandle>>;

/// Lets type-erased callers (the layout editor, dock migration) wrap a panel in a
/// [`PanelItem`], which otherwise needs the panel's concrete type.
#[derive(Default)]
pub(crate) struct PanelItemRegistry {
    builders: HashMap<&'static str, PanelItemBuilder>,
    item_types: collections::HashSet<TypeId>,
}

impl Global for PanelItemRegistry {}

impl PanelItemRegistry {
    pub(crate) fn register<T: Panel>(cx: &mut App) {
        let builder: PanelItemBuilder = |panel, window, cx| {
            let panel = panel.to_any().downcast::<T>().ok()?;
            let item = cx.new(|cx| PanelItem::new(panel, window, cx));
            Some(Box::new(item) as Box<dyn ItemHandle>)
        };
        let registry = cx.default_global::<Self>();
        registry.builders.insert(T::persistent_name(), builder);
        registry.item_types.insert(TypeId::of::<PanelItem<T>>());
    }

    pub(crate) fn is_panel_item(view: &AnyView, cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|registry| registry.item_types.contains(&view.entity_type()))
    }

    pub(crate) fn is_panel_kind(kind: &str, cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|registry| registry.builders.contains_key(kind))
    }

    pub(crate) fn build(
        panel: &Arc<dyn PanelHandle>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Box<dyn ItemHandle>> {
        let builder = *cx
            .try_global::<Self>()?
            .builders
            .get(panel.persistent_name())?;
        builder(panel, window, cx)
    }
}

/// Hosts a [`Panel`] as an [`Item`], so panels can live anywhere in the pane tree
/// instead of only inside a [`crate::Dock`].
///
/// The panel entity is owned by [`Workspace`], never by this wrapper: closing the tab
/// must not drop the panel, because panels such as the web preview and the iOS
/// simulator own native views whose state cannot be rebuilt.
pub struct PanelItem<T: Panel> {
    panel: Entity<T>,
    _subscription: Subscription,
}

impl<T: Panel> PanelItem<T> {
    pub fn new(panel: Entity<T>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _subscription = cx.subscribe_in(&panel, window, |_, _, event, window, cx| {
            match event {
                // A panel's own zoom button predates panes; route it to the hosting
                // pane so both paths end up toggling the same zoom state.
                PanelEvent::ZoomIn | PanelEvent::ZoomOut => {
                    window.dispatch_action(Box::new(ToggleZoom), cx);
                }
                PanelEvent::Activate | PanelEvent::Close => {}
            }
            cx.emit(*event);
        });

        Self {
            panel,
            _subscription,
        }
    }

    pub fn panel(&self) -> &Entity<T> {
        &self.panel
    }

    /// `tab_content_text` has no `Window`, so it falls back to the panel's stable name
    /// rather than duplicating [`PanelHandle::tab_label`]'s derivation.
    fn label(&self, window: Option<&Window>, cx: &App) -> SharedString {
        match window {
            Some(window) => self.panel.tab_label(window, cx),
            None => SharedString::from(T::persistent_name()),
        }
    }
}

impl<T: Panel> Focusable for PanelItem<T> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // `Pane` uses an item's focus handle both to focus it and to test containment, so this
        // returns the panel's activation handle (e.g. a commit or filter editor) to keep focusing
        // a panel tab equivalent to activating it in a dock.
        self.panel.read(cx).activation_focus_handle(cx)
    }
}

impl<T: Panel> EventEmitter<PanelEvent> for PanelItem<T> {}

impl<T: Panel> Render for PanelItem<T> {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        AnyView::from(self.panel.clone())
            .cached(StyleRefinement::default().v_flex().size_full())
    }
}

impl<T: Panel> Item for PanelItem<T> {
    type Event = PanelEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.label(None, cx)
    }

    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> AnyElement {
        Label::new(self.label(Some(window), cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_icon(&self, window: &Window, cx: &App) -> Option<Icon> {
        self.panel.read(cx).icon(window, cx).map(Icon::new)
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            PanelEvent::Close => f(ItemEvent::CloseItem),
            PanelEvent::Activate => f(ItemEvent::UpdateTab),
            PanelEvent::ZoomIn | PanelEvent::ZoomOut => {}
        }
    }

    fn activated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.set_active(true, window, cx));
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.set_active(false, window, cx));
    }

    fn content_kind(&self, _cx: &App) -> Option<crate::pane::ContentKind> {
        Some(crate::pane::ContentKind::panel(T::persistent_name()))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        workspace.register_panel_item_instance(self.panel.entity_id(), cx.entity_id());
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn include_in_nav_history() -> bool {
        false
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<T>() {
            Some(self.panel.clone().into())
        } else {
            None
        }
    }
}

impl<T: Panel> SerializableItem for PanelItem<T> {
    fn serialized_item_kind() -> &'static str {
        T::persistent_name()
    }

    fn cleanup(
        _workspace_id: WorkspaceId,
        _alive_items: Vec<ItemId>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    /// The panel is a workspace singleton, so restoring a tab means re-wrapping the
    /// existing entity rather than rebuilding state from the database.
    fn deserialize(
        _project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        _workspace_id: WorkspaceId,
        _item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let panel = workspace.update(cx, |workspace, cx| workspace.panel::<T>(cx));
        Task::ready(match panel {
            Ok(Some(panel)) => Ok(cx.new(|cx| Self::new(panel, window, cx))),
            Ok(None) => Err(anyhow!(
                "cannot restore {}, panel is not registered in the workspace",
                T::persistent_name()
            )),
            Err(error) => Err(error),
        })
    }

    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        _item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        None
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}
