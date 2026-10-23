use crate::{DeviceStatus, render_device_status_chip};
use gpui::{
    Anchor, AnyElement, ClickEvent, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, ScrollHandle, Subscription, WeakEntity, px,
};
use ios_simulator::{
    Appearance, IosSimulatorManager, LANGUAGES, PrepareRequest, ProductFamily, RuntimeOption,
    SimDevice, SimDeviceType, SimLanguage, SimulatorSettings, compare_versions, display_version,
    format_bytes, runtime_version_from_identifier, version_key,
};
use std::collections::HashMap;
use ui::{
    CommonAnimationExt as _, ContextMenu, ContextMenuEntry, PopoverMenu, PopoverMenuHandle,
    Tooltip, checkbox_indicator, destructive_button, modal_footer, modal_surface, prelude::*,
    primary_button, radio_indicator, secondary_button, segment_option, segmented_control,
    toggle_switch,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{ModalView, Workspace, notifications::NotifyTaskExt as _};

/// How many runtimes step D lists before "Mostrar versões mais antigas".
const RECENT_RUNTIME_COUNT: usize = 5;
const MODEL_COLUMNS: usize = 4;

/// Where the modal opens.
pub enum IosManagerEntry {
    Simulators,
    NewSimulator,
}

/// The simulator manager of the iOS devices panel: simulators and runtimes (boards B and H), the
/// delete and erase confirmations (G), the new-simulator wizard (C, D, E) and the edit form (I),
/// all in one modal so moving between them never closes it.
pub struct IosDeviceManagerModal {
    manager: Entity<IosSimulatorManager>,
    workspace: WeakEntity<Workspace>,
    mode: Mode,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    row_menus: HashMap<String, PopoverMenuHandle<ContextMenu>>,
    name_input: Entity<InputField>,
    search_input: Entity<InputField>,
    _subscriptions: Vec<Subscription>,
}

enum Mode {
    Manage(ManagerTab),
    Confirm(Confirmation),
    Wizard(Wizard),
    Edit(EditForm),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagerTab {
    Simulators,
    Runtimes,
}

enum Confirmation {
    Delete { udid: String, remove_runtime: bool },
    Erase { udid: String },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WizardStep {
    Model,
    Runtime,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WizardPurpose {
    Create,
    /// "Baixar runtime…" in the runtimes tab: step D alone.
    DownloadRuntime,
}

struct Wizard {
    step: WizardStep,
    purpose: WizardPurpose,
    family: ProductFamily,
    device_type: Option<String>,
    show_prerelease: bool,
    runtime: Option<(String, String)>,
    show_older_runtimes: bool,
    language: SimLanguage,
    appearance: Appearance,
    boot_when_ready: bool,
}

struct EditForm {
    udid: String,
    language: SimLanguage,
    appearance: Appearance,
}

impl EventEmitter<DismissEvent> for IosDeviceManagerModal {}
impl ModalView for IosDeviceManagerModal {}

impl Focusable for IosDeviceManagerModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl IosDeviceManagerModal {
    pub fn new(
        manager: Entity<IosSimulatorManager>,
        workspace: WeakEntity<Workspace>,
        entry: IosManagerEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        manager.update(cx, |manager, cx| {
            manager.refresh(cx);
            manager.refresh_catalog(cx);
        });
        let name_input = cx.new(|cx| InputField::new(window, cx, "iPhone 17 Pro").label("Nome"));
        let search_input = cx.new(|cx| {
            InputField::new(window, cx, "Buscar modelo").start_icon(IconName::MagnifyingGlass)
        });
        let mut subscriptions = vec![cx.observe(&manager, |this, _, cx| {
            this.fill_wizard_defaults(cx);
            cx.notify();
        })];
        for input in [&name_input, &search_input] {
            let editor = input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |_, cx| cx.notify()).log_err();
                    }
                }),
                window,
                cx,
            ));
        }
        let mut this = Self {
            manager,
            workspace,
            mode: Mode::Manage(ManagerTab::Simulators),
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            row_menus: HashMap::new(),
            name_input,
            search_input,
            _subscriptions: subscriptions,
        };
        if matches!(entry, IosManagerEntry::NewSimulator) {
            this.start_wizard(WizardPurpose::Create, None, window, cx);
        }
        this
    }

    fn start_wizard(
        &mut self,
        purpose: WizardPurpose,
        device_type: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let family = device_type
            .as_deref()
            .and_then(|identifier| self.manager.read(cx).device_type(identifier))
            .map_or(ProductFamily::IPhone, |device_type| device_type.family);
        let name = device_type
            .as_deref()
            .and_then(|identifier| self.manager.read(cx).device_type(identifier))
            .map(|device_type| device_type.name.clone())
            .unwrap_or_default();
        let step = match (purpose, &device_type) {
            (WizardPurpose::DownloadRuntime, _) | (WizardPurpose::Create, Some(_)) => {
                WizardStep::Runtime
            }
            (WizardPurpose::Create, None) => WizardStep::Model,
        };
        self.mode = Mode::Wizard(Wizard {
            step,
            purpose,
            family,
            device_type,
            show_prerelease: false,
            runtime: None,
            show_older_runtimes: false,
            language: LANGUAGES[0],
            appearance: Appearance::Dark,
            boot_when_ready: true,
        });
        self.name_input
            .update(cx, |input, cx| input.set_text(&name, window, cx));
        self.search_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.fill_wizard_defaults(cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn start_edit(&mut self, udid: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(device) = self.manager.read(cx).device(&udid).cloned() else {
            return;
        };
        self.name_input
            .update(cx, |input, cx| input.set_text(&device.name, window, cx));
        self.mode = Mode::Edit(EditForm {
            udid: udid.clone(),
            language: LANGUAGES[0],
            appearance: Appearance::Light,
        });
        let read = self.manager.read(cx).read_settings(&udid, cx);
        cx.spawn(async move |this, cx| {
            let (language, appearance) = read.await;
            this.update(cx, |this, cx| {
                if let Mode::Edit(form) = &mut this.mode
                    && form.udid == udid
                {
                    if let Some(language) = language {
                        form.language = language;
                    }
                    form.appearance = appearance;
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Picks the model and runtime the wizard starts on once the lists have loaded, without
    /// overriding a choice already made.
    fn fill_wizard_defaults(&mut self, cx: &mut Context<Self>) {
        let Mode::Wizard(wizard) = &mut self.mode else {
            return;
        };
        let manager = self.manager.read(cx);
        if wizard.device_type.is_none() && wizard.purpose == WizardPurpose::Create {
            wizard.device_type = visible_models(manager, wizard.family, "")
                .first()
                .map(|device_type| device_type.identifier.clone());
        }
        let model = wizard
            .device_type
            .as_deref()
            .and_then(|identifier| manager.device_type(identifier));
        let options = manager.runtime_options(model);
        let still_valid = wizard.runtime.as_ref().is_some_and(|(_, build)| {
            options
                .iter()
                .any(|option| &option.build == build && option.unavailable.is_none())
        });
        if !still_valid {
            let downloading = wizard.purpose == WizardPurpose::DownloadRuntime;
            let usable = |option: &&RuntimeOption| {
                option.unavailable.is_none()
                    && option.prerelease == wizard.show_prerelease
                    && !(downloading && option.installed)
            };
            // Creating starts on what is already on the Mac; downloading on what Xcode asks for.
            let preferred = |option: &&RuntimeOption| {
                if downloading {
                    option.requested
                } else {
                    option.installed
                }
            };
            wizard.runtime = options
                .iter()
                .filter(usable)
                .find(preferred)
                .or_else(|| options.iter().find(|option| usable(option)))
                .map(|option| (option.version.clone(), option.build.clone()));
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        match &mut self.mode {
            Mode::Manage(_) => self.dismiss(cx),
            Mode::Confirm(_) | Mode::Edit(_) => self.mode = Mode::Manage(ManagerTab::Simulators),
            Mode::Wizard(wizard) => match (wizard.step, wizard.purpose) {
                (WizardStep::Settings, WizardPurpose::Create) => wizard.step = WizardStep::Runtime,
                (WizardStep::Runtime, WizardPurpose::Create) => wizard.step = WizardStep::Model,
                (_, WizardPurpose::DownloadRuntime) => {
                    self.mode = Mode::Manage(ManagerTab::Runtimes)
                }
                _ => self.mode = Mode::Manage(ManagerTab::Simulators),
            },
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Manage(_) => {}
            Mode::Confirm(_) => self.run_confirmation(window, cx),
            Mode::Wizard(_) => self.advance_wizard(window, cx),
            Mode::Edit(_) => self.save_edit(window, cx),
        }
    }

    fn notify_err(&self, task: gpui::Task<anyhow::Result<()>>, window: &mut Window, cx: &mut App) {
        task.detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    fn run_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Confirm(confirmation) = &self.mode else {
            return;
        };
        let task = match confirmation {
            Confirmation::Delete {
                udid,
                remove_runtime,
            } => {
                let (udid, remove_runtime) = (udid.clone(), *remove_runtime);
                self.manager.update(cx, |manager, cx| {
                    manager.delete_device(udid, remove_runtime, cx)
                })
            }
            Confirmation::Erase { udid } => {
                let udid = udid.clone();
                self.manager
                    .update(cx, |manager, cx| manager.erase_device(udid, cx))
            }
        };
        self.notify_err(task, window, cx);
        self.mode = Mode::Manage(ManagerTab::Simulators);
        cx.notify();
    }

    fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Edit(form) = &self.mode else {
            return;
        };
        let name = self.name_input.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            return;
        }
        let (udid, language, appearance) = (form.udid.clone(), form.language, form.appearance);
        let task = self.manager.update(cx, |manager, cx| {
            manager.save_settings(udid, name, language, appearance, cx)
        });
        self.notify_err(task, window, cx);
        self.mode = Mode::Manage(ManagerTab::Simulators);
        cx.notify();
    }

    fn wizard_settings(&self, wizard: &Wizard, cx: &App) -> Option<SimulatorSettings> {
        let name = self.name_input.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            return None;
        }
        let (runtime_version, runtime_build) = wizard.runtime.clone()?;
        Some(SimulatorSettings {
            name,
            device_type: wizard.device_type.clone()?,
            runtime_version,
            runtime_build,
            language: wizard.language,
            appearance: wizard.appearance,
        })
    }

    fn short_on_space(&self, wizard: &Wizard, cx: &App) -> bool {
        let manager = self.manager.read(cx);
        let download = selected_option(manager, wizard).and_then(|option| option.download_bytes);
        match (manager.available_bytes(), download) {
            (Some(available), Some(required)) => available < required,
            _ => false,
        }
    }

    fn can_advance(&self, wizard: &Wizard, cx: &App) -> bool {
        match wizard.step {
            WizardStep::Model => wizard.device_type.is_some(),
            WizardStep::Runtime => match wizard.purpose {
                WizardPurpose::Create => wizard.runtime.is_some(),
                WizardPurpose::DownloadRuntime => self
                    .manager
                    .read(cx)
                    .runtime_options(None)
                    .iter()
                    .any(|option| {
                        Some(&option.build) == wizard.runtime.as_ref().map(|(_, build)| build)
                            && !option.installed
                    }),
            },
            WizardStep::Settings => {
                self.wizard_settings(wizard, cx).is_some() && !self.short_on_space(wizard, cx)
            }
        }
    }

    fn advance_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Wizard(wizard) = &self.mode else {
            return;
        };
        if !self.can_advance(wizard, cx) || self.manager.read(cx).is_busy() {
            return;
        }
        let request = match (wizard.step, wizard.purpose) {
            (WizardStep::Model, _) => {
                let model_name = wizard
                    .device_type
                    .as_deref()
                    .and_then(|identifier| self.manager.read(cx).device_type(identifier))
                    .map(|device_type| device_type.name.clone());
                if let Some(name) = model_name
                    && self.name_input.read(cx).is_empty(cx)
                {
                    self.name_input
                        .update(cx, |input, cx| input.set_text(&name, window, cx));
                }
                self.set_wizard_step(WizardStep::Runtime, cx);
                return;
            }
            (WizardStep::Runtime, WizardPurpose::DownloadRuntime) => {
                let Some((version, build)) = wizard.runtime.clone() else {
                    return;
                };
                PrepareRequest::DownloadRuntime { version, build }
            }
            (WizardStep::Runtime, WizardPurpose::Create) => {
                self.set_wizard_step(WizardStep::Settings, cx);
                return;
            }
            (WizardStep::Settings, _) => {
                let Some(settings) = self.wizard_settings(wizard, cx) else {
                    return;
                };
                PrepareRequest::Create {
                    settings,
                    boot_when_ready: wizard.boot_when_ready,
                }
            }
        };
        self.manager
            .update(cx, |manager, cx| manager.prepare(request, cx));
        window.focus(&self.focus_handle, cx);
        self.dismiss(cx);
    }

    fn set_wizard_step(&mut self, step: WizardStep, cx: &mut Context<Self>) {
        if let Mode::Wizard(wizard) = &mut self.mode {
            wizard.step = step;
            self.scroll_handle.set_offset(gpui::Point::default());
            cx.notify();
        }
    }

    fn update_wizard(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut Wizard)) {
        if let Mode::Wizard(wizard) = &mut self.mode {
            update(wizard);
            cx.notify();
        }
    }

    fn update_edit(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut EditForm)) {
        if let Mode::Edit(form) = &mut self.mode {
            update(form);
            cx.notify();
        }
    }

    fn row_menu_handle(&mut self, udid: &str) -> PopoverMenuHandle<ContextMenu> {
        self.row_menus.entry(udid.to_string()).or_default().clone()
    }

    fn render_header(
        &self,
        title: SharedString,
        subtitle: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        h_flex()
            .w_full()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new(title)
                            .size(LabelSize::Large)
                            .weight(FontWeight::SEMIBOLD)
                            .truncate(),
                    )
                    .children(subtitle.map(|subtitle| {
                        Label::new(subtitle)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .child(
                IconButton::new("ios-manager-close", IconName::Close)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .aria_label("Fechar")
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
            )
    }

    fn render_manage(&mut self, tab: ManagerTab, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let mut subtitle = format!(
            "{} · {}",
            plural(manager.devices().len(), "simulador", "simuladores"),
            plural(
                manager.runtimes().len(),
                "runtime instalado",
                "runtimes instalados"
            )
        );
        if let Some(version) = manager.xcode().and_then(|xcode| xcode.version()) {
            subtitle.push_str(&format!(" · Xcode {version}"));
        }
        let tabs = segmented_control(cx).w(px(220.)).children(
            [
                (ManagerTab::Simulators, "Simuladores"),
                (ManagerTab::Runtimes, "Runtimes"),
            ]
            .into_iter()
            .map(|(option, label)| {
                segment_option(
                    ("ios-manager-tab", option as usize),
                    label,
                    option == tab,
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mode = Mode::Manage(option);
                    cx.notify();
                }))
            }),
        );
        let header = v_flex()
            .w_full()
            .gap(px(14.))
            .pt(px(20.))
            .pr(px(20.))
            .pb(px(14.))
            .pl_6()
            .child(self.render_header("Simuladores iOS".into(), Some(subtitle.into()), cx))
            .child(tabs);
        let body = match tab {
            ManagerTab::Simulators => self.render_simulators_tab(cx),
            ManagerTab::Runtimes => self.render_runtimes_tab(cx),
        };
        modal_surface(cx)
            .w(px(700.))
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_simulators_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let devices = manager.devices().to_vec();
        let busy = manager.is_busy();
        let devices_dir = manager.devices_dir();
        let footer_text = format!(
            "{} · {} em uso{}",
            tildify(devices_dir.parent().unwrap_or(&devices_dir)),
            format_bytes(manager.devices_disk_bytes()),
            manager
                .available_bytes()
                .map(|available| format!(" · {} livres", format_bytes(available)))
                .unwrap_or_default()
        );
        let families: HashMap<String, ProductFamily> = manager
            .device_types()
            .iter()
            .map(|device_type| (device_type.identifier.clone(), device_type.family))
            .collect();

        let list = v_flex()
            .id("ios-simulator-list")
            .w_full()
            .max_h(px(420.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .gap_0p5()
            .pt_1()
            .px_3()
            .pb_3()
            .when(devices.is_empty(), |this| {
                this.child(
                    v_flex()
                        .py_8()
                        .items_center()
                        .gap_1()
                        .child(Label::new("Nenhum simulador ainda"))
                        .child(
                            Label::new("Crie um para rodar apps iOS no dock.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .children(devices.iter().enumerate().map(|(index, device)| {
                let family = families.get(&device.device_type_identifier).copied();
                let menu_handle = self.row_menu_handle(&device.udid);
                self.render_device_row(index, device, family, busy, menu_handle, cx)
            }));

        let open_dir = devices_dir;
        let footer = modal_footer(cx)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::XSmall)
                            .color(Color::Placeholder),
                    )
                    .child(
                        Label::new(footer_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder)
                            .truncate(),
                    ),
            )
            .child(
                secondary_button("ios-open-folder", None, "Abrir pasta", true, cx)
                    .on_click(move |_, _, cx| cx.open_with_system(&open_dir)),
            )
            .child(
                primary_button("ios-new-simulator", None, "Novo simulador", None, cx).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.start_wizard(WizardPurpose::Create, None, window, cx)
                    }),
                ),
            );
        v_flex()
            .w_full()
            .child(list)
            .child(footer)
            .into_any_element()
    }

    fn render_device_row(
        &self,
        index: usize,
        device: &SimDevice,
        family: Option<ProductFamily>,
        busy: bool,
        menu_handle: PopoverMenuHandle<ContextMenu>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors().clone();
        let menu_open = menu_handle.is_deployed();
        let status = device_status(device);
        let is_running = device.is_running();
        let runtime_missing = !device.is_available;
        let version = device
            .runtime_version()
            .map(|version| format!("iOS {}", display_version(&version)))
            .unwrap_or_else(|| "iOS desconhecido".to_string());
        let mut meta = version;
        if let Some(family) = family {
            meta.push_str(" · ");
            meta.push_str(family.label());
        }
        meta.push_str(" · ");
        if runtime_missing {
            meta.push_str("runtime removido do Mac");
        } else {
            meta.push_str(&format_bytes(device.disk_bytes));
        }

        let udid = device.udid.clone();
        let (action_icon, action_color, action_label) = if is_running {
            (IconName::Stop, Color::Error, "Parar")
        } else if runtime_missing {
            (IconName::Download, Color::Default, "Baixar o runtime")
        } else {
            (IconName::PlayFilled, Color::Success, "Iniciar")
        };
        let action = IconButton::new(("ios-device-action", index), action_icon)
            .icon_size(IconSize::Small)
            .icon_color(action_color)
            .aria_label(action_label)
            .tooltip(Tooltip::text(action_label))
            .disabled(busy && !is_running)
            .on_click(cx.listener({
                let udid = udid.clone();
                let runtime_version = device.runtime_version();
                move |this, _, window, cx| {
                    let udid = udid.clone();
                    if is_running {
                        this.manager
                            .update(cx, |manager, cx| manager.stop_device(udid, cx));
                    } else if runtime_missing {
                        if let Some(version) = runtime_version.clone() {
                            this.download_missing_runtime(&version, window, cx);
                        }
                    } else {
                        this.manager
                            .update(cx, |manager, cx| manager.boot_device(udid, cx));
                        this.dismiss(cx);
                    }
                }
            }));

        let this = cx.weak_entity();
        let name = device.name.clone();
        let more = PopoverMenu::new(("ios-device-menu", index))
            .with_handle(menu_handle)
            .anchor(Anchor::TopRight)
            .trigger(
                IconButton::new(("ios-device-more", index), IconName::Ellipsis)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .aria_label(format!("Ações de {name}")),
            )
            .menu(move |window, cx| {
                let this = this.clone();
                let udid = udid.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let off_disk_disabled = is_running || busy;
                    let item = |label: &'static str,
                                icon: IconName,
                                disabled: bool,
                                action: fn(
                        &mut IosDeviceManagerModal,
                        String,
                        &mut Window,
                        &mut Context<IosDeviceManagerModal>,
                    )| {
                        let this = this.clone();
                        let udid = udid.clone();
                        ContextMenuEntry::new(label)
                            .icon(icon)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .disabled(disabled)
                            .handler(move |window, cx| {
                                let udid = udid.clone();
                                this.update(cx, |this, cx| action(this, udid, window, cx))
                                    .log_err();
                            })
                    };
                    let delete = {
                        let this = this.clone();
                        let udid = udid.clone();
                        move |window: &mut Window, cx: &mut App| {
                            let udid = udid.clone();
                            this.update(cx, |this, cx| {
                                let remove_runtime = this.runtime_alone(&udid, cx);
                                this.mode = Mode::Confirm(Confirmation::Delete {
                                    udid,
                                    remove_runtime,
                                });
                                window.focus(&this.focus_handle, cx);
                                cx.notify();
                            })
                            .log_err();
                        }
                    };
                    menu.item(item(
                        "Iniciar",
                        IconName::PlayFilled,
                        is_running || runtime_missing,
                        |this, udid, _, cx| {
                            this.manager
                                .update(cx, |manager, cx| manager.boot_device(udid, cx));
                            this.dismiss(cx);
                        },
                    ))
                    .item(item(
                        "Copiar UDID",
                        IconName::Copy,
                        false,
                        |_, udid, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(udid));
                        },
                    ))
                    .item(item(
                        "Editar…",
                        IconName::Pencil,
                        busy || runtime_missing,
                        |this, udid, window, cx| this.start_edit(udid, window, cx),
                    ))
                    .item(item(
                        "Duplicar",
                        IconName::Copy,
                        off_disk_disabled || runtime_missing,
                        |this, udid, window, cx| {
                            let task = this
                                .manager
                                .update(cx, |manager, cx| manager.duplicate_device(udid, cx));
                            this.notify_err(task, window, cx);
                        },
                    ))
                    .item(item(
                        "Apagar conteúdo e ajustes…",
                        IconName::Eraser,
                        off_disk_disabled || runtime_missing,
                        |this, udid, window, cx| {
                            this.mode = Mode::Confirm(Confirmation::Erase { udid });
                            window.focus(&this.focus_handle, cx);
                            cx.notify();
                        },
                    ))
                    .item(item(
                        "Mostrar no Finder",
                        IconName::Folder,
                        false,
                        |this, udid, _, cx| {
                            if let Some(directory) = this
                                .manager
                                .read(cx)
                                .device(&udid)
                                .and_then(SimDevice::directory)
                            {
                                cx.reveal_path(&directory);
                            }
                        },
                    ))
                    .separator()
                    .custom_entry(
                        move |_, _| {
                            h_flex()
                                .gap_2()
                                .opacity(if off_disk_disabled { 0.5 } else { 1.0 })
                                .child(
                                    Icon::new(IconName::Trash)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                )
                                .child(Label::new("Excluir…").color(Color::Error))
                                .into_any_element()
                        },
                        move |window, cx| {
                            if !off_disk_disabled {
                                delete(window, cx);
                            }
                        },
                    )
                }))
            });

        h_flex()
            .id(("ios-device-row", index))
            .w_full()
            .h(px(60.))
            .pl(px(14.))
            .pr_3()
            .gap_3()
            .rounded_lg()
            .when(menu_open, |this| this.bg(colors.ghost_element_hover))
            .hover(|this| this.bg(colors.ghost_element_hover))
            .when(is_running, |this| {
                this.tooltip(Tooltip::text("Pare o simulador primeiro"))
            })
            .child(
                div()
                    .size(px(32.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded_lg()
                    .bg(colors.element_hover)
                    .child(Icon::new(IconName::Smartphone).size(IconSize::Small)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(device.name.clone())
                                    .weight(FontWeight::SEMIBOLD)
                                    .truncate(),
                            )
                            .child(render_device_status_chip(status, cx)),
                    )
                    .child(
                        Label::new(meta)
                            .size(LabelSize::Small)
                            .color(if runtime_missing {
                                Color::Warning
                            } else {
                                Color::Muted
                            })
                            .truncate(),
                    ),
            )
            .child(action)
            .child(more)
    }

    /// Whether `udid` is the only simulator of its runtime, so deleting it can take the
    /// runtime along.
    fn runtime_alone(&self, udid: &str, cx: &App) -> bool {
        let manager = self.manager.read(cx);
        manager
            .device(udid)
            .and_then(|device| manager.runtime(&device.runtime_identifier))
            .is_some_and(|runtime| !manager.runtime_used_by_other_device(&runtime.identifier, udid))
    }

    /// Downloads the runtime a simulator lost, when Apple still offers that version.
    fn download_missing_runtime(
        &mut self,
        version: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let runtime = self.manager.read(cx).catalog().and_then(|catalog| {
            catalog
                .runtimes
                .iter()
                .find(|runtime| {
                    !runtime.prerelease
                        && compare_versions(&runtime.version, version) == std::cmp::Ordering::Equal
                })
                .cloned()
        });
        match runtime {
            Some(runtime) => {
                self.manager.update(cx, |manager, cx| {
                    manager.prepare(
                        PrepareRequest::DownloadRuntime {
                            version: runtime.version,
                            build: runtime.build,
                        },
                        cx,
                    )
                });
                self.dismiss(cx);
            }
            None => {
                let task = gpui::Task::ready(Err(anyhow::anyhow!(
                    "A Apple não oferece mais o iOS {}; exclua o simulador ou crie outro.",
                    display_version(version)
                )));
                self.notify_err(task, window, cx);
            }
        }
    }

    fn render_runtimes_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let busy = manager.is_busy();
        let colors = cx.theme().colors().clone();
        let mut runtimes: Vec<(String, String, Vec<String>, Option<u64>)> = manager
            .runtimes()
            .iter()
            .map(|runtime| {
                (
                    display_version(&runtime.version),
                    runtime.build.clone(),
                    manager.runtime_used_by(runtime),
                    manager.runtime_disk_bytes(runtime),
                )
            })
            .collect();
        runtimes.sort_by(|left, right| compare_versions(&right.0, &left.0));
        let requested = manager.requested_runtime();
        let xcode = manager.xcode().cloned().unwrap_or_default();
        let section = |title: &'static str| {
            Label::new(title)
                .size(LabelSize::XSmall)
                .weight(FontWeight::SEMIBOLD)
                .color(Color::Placeholder)
        };
        let table = || {
            v_flex()
                .w_full()
                .rounded_lg()
                .border_1()
                .border_color(colors.border)
                .overflow_hidden()
        };

        let runtime_rows = runtimes
            .iter()
            .enumerate()
            .map(|(index, (version, build, used_by, disk_bytes))| {
                let unused = used_by.is_empty();
                let usage = match used_by.as_slice() {
                    [] => "nenhum simulador usa".to_string(),
                    [name] => format!("usado por {name}"),
                    names => format!("usado por {} simuladores", names.len()),
                };
                let build_to_delete = build.clone();
                h_flex()
                    .w_full()
                    .h(px(44.))
                    .pl(px(14.))
                    .pr_2()
                    .gap_3()
                    .when(index > 0, |this| {
                        this.border_t_1().border_color(colors.border)
                    })
                    .child(
                        v_flex()
                            .w(px(200.))
                            .flex_none()
                            .child(
                                Label::new(format!("iOS {version}"))
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(format!("Build {build} · arm64"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(usage)
                                .size(LabelSize::Small)
                                .color(if unused { Color::Warning } else { Color::Muted })
                                .truncate(),
                        ),
                    )
                    .children(disk_bytes.map(|bytes| {
                        Label::new(format_bytes(bytes))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    }))
                    .child(
                        IconButton::new(("ios-remove-runtime", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .icon_color(if unused {
                                Color::Error
                            } else {
                                Color::Placeholder
                            })
                            .aria_label("Remover runtime")
                            .disabled(!unused || busy)
                            .tooltip(Tooltip::text(if unused {
                                "Remover runtime"
                            } else {
                                "Algum simulador usa este runtime"
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let build = build_to_delete.clone();
                                let task = this
                                    .manager
                                    .update(cx, |manager, cx| manager.delete_runtime(build, cx));
                                this.notify_err(task, window, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();

        let xcode_row = |index: usize, label: String, value: String, end: AnyElement| {
            h_flex()
                .w_full()
                .h(px(40.))
                .pl(px(14.))
                .pr(px(10.))
                .gap_3()
                .when(index > 0, |this| {
                    this.border_t_1().border_color(colors.border)
                })
                .child(
                    div().w(px(200.)).flex_none().child(
                        Label::new(label)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    ),
                )
                .child(
                    Label::new(value)
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(Color::Muted),
                )
                .child(div().flex_1())
                .child(end)
        };
        let mut xcode_rows = Vec::new();
        if let Some((runtime, installed)) = requested {
            let end =
                if installed {
                    Label::new("instalado")
                        .size(LabelSize::Small)
                        .color(Color::Placeholder)
                        .into_any_element()
                } else {
                    let (version, build) = (runtime.version.clone(), runtime.build.clone());
                    h_flex()
                        .gap_3()
                        .child(
                            Label::new(format!(
                                "não instalado · {}",
                                format_bytes(runtime.download_bytes)
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                        )
                        .child(
                            secondary_button("ios-download-requested", None, "Baixar", !busy, cx)
                                .when(!busy, |this| {
                                    this.on_click(cx.listener(move |this, _, _, cx| {
                                        let request = PrepareRequest::DownloadRuntime {
                                            version: version.clone(),
                                            build: build.clone(),
                                        };
                                        this.manager
                                            .update(cx, |manager, cx| manager.prepare(request, cx));
                                        this.dismiss(cx);
                                    }))
                                }),
                        )
                        .into_any_element()
                };
            xcode_rows.push(xcode_row(
                xcode_rows.len(),
                format!(
                    "iOS {} · pedido pelo Xcode",
                    display_version(&runtime.version)
                ),
                runtime.build,
                end,
            ));
        }
        if let Some((version, build)) = &xcode.xcode_version {
            xcode_rows.push(xcode_row(
                xcode_rows.len(),
                "Xcode".to_string(),
                format!("{version} · {build}"),
                Label::new("atualiza pela App Store")
                    .size(LabelSize::Small)
                    .color(Color::Placeholder)
                    .into_any_element(),
            ));
        }
        if let Some(developer_dir) = &xcode.developer_dir {
            let app = developer_dir
                .ancestors()
                .find(|path| path.extension().is_some_and(|extension| extension == "app"))
                .unwrap_or(developer_dir);
            xcode_rows.push(xcode_row(
                xcode_rows.len(),
                "xcode-select".to_string(),
                app.display().to_string(),
                Label::new("ok")
                    .size(LabelSize::Small)
                    .color(Color::Placeholder)
                    .into_any_element(),
            ));
        }

        let body = v_flex()
            .id("ios-runtimes-body")
            .w_full()
            .max_h(px(480.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .gap_4()
            .pt_1()
            .px_6()
            .pb_5()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(section("RUNTIMES INSTALADOS"))
                    .child(
                        table()
                            .when(runtimes.is_empty(), |this| {
                                this.child(
                                    div().p_3().child(
                                        Label::new("Nenhum runtime instalado")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                                )
                            })
                            .children(runtime_rows),
                    ),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(section("XCODE"))
                    .child(table().children(xcode_rows)),
            );

        let footer =
            modal_footer(cx)
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::XSmall)
                                .color(Color::Placeholder),
                        )
                        .child(
                            Label::new(
                                "Runtimes só são removidos quando nenhum simulador depende deles.",
                            )
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder)
                            .truncate(),
                        ),
                )
                .child(
                    secondary_button("ios-download-runtime", None, "Baixar runtime…", !busy, cx)
                        .when(!busy, |this| {
                            this.on_click(cx.listener(|this, _, window, cx| {
                                this.start_wizard(WizardPurpose::DownloadRuntime, None, window, cx)
                            }))
                        }),
                );
        v_flex()
            .w_full()
            .child(body)
            .child(footer)
            .into_any_element()
    }

    fn render_confirmation(&self, cx: &mut Context<Self>) -> AnyElement {
        let Mode::Confirm(confirmation) = &self.mode else {
            return div().into_any_element();
        };
        let manager = self.manager.read(cx);
        let (udid, erase) = match confirmation {
            Confirmation::Delete { udid, .. } => (udid, false),
            Confirmation::Erase { udid } => (udid, true),
        };
        let Some(device) = manager.device(udid).cloned() else {
            return div().into_any_element();
        };
        let remove_runtime = matches!(
            confirmation,
            Confirmation::Delete {
                remove_runtime: true,
                ..
            }
        );
        let runtime = manager.runtime(&device.runtime_identifier).cloned();
        let runtime_alone = runtime.as_ref().is_some_and(|runtime| {
            !manager.runtime_used_by_other_device(&runtime.identifier, udid)
        });
        let runtime_bytes = runtime
            .as_ref()
            .and_then(|runtime| manager.runtime_disk_bytes(runtime))
            .unwrap_or(0);
        let family = manager
            .device_type(&device.device_type_identifier)
            .map(|device_type| device_type.family.label());
        let version = device
            .runtime_version()
            .map(|version| display_version(&version))
            .unwrap_or_default();
        let mut subtitle = format!("iOS {version}");
        if let Some(family) = family {
            subtitle.push_str(" · ");
            subtitle.push_str(family);
        }
        if !erase && runtime_alone {
            subtitle.push_str(" · último simulador desse runtime");
        }
        let error = Color::Error.color(cx);
        let consequence = |icon: IconName, text: String, detail: String| {
            h_flex()
                .w_full()
                .gap_2p5()
                .items_start()
                .child(
                    div()
                        .pt_0p5()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted)),
                )
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(Label::new(text).size(LabelSize::Small))
                        .child(
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
        };
        let freed = device.disk_bytes + if remove_runtime { runtime_bytes } else { 0 };
        let (title, action_label) = if erase {
            (
                format!("Apagar conteúdo e ajustes de {}?", device.name),
                "Apagar conteúdo e ajustes".to_string(),
            )
        } else {
            (
                format!("Excluir {}?", device.name),
                format!("Excluir e liberar {}", format_bytes(freed)),
            )
        };
        let colors = cx.theme().colors().clone();
        let devices_dir = tildify(&manager.devices_dir());
        let content = v_flex()
            .w_full()
            .gap_4()
            .pt(px(22.))
            .px_6()
            .pb(px(18.))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .size_9()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(error.opacity(0.12))
                            .child(
                                Icon::new(if erase {
                                    IconName::Eraser
                                } else {
                                    IconName::Trash
                                })
                                .size(IconSize::Medium)
                                .color(Color::Error),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                Label::new(title)
                                    .size(LabelSize::Large)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(subtitle)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_3()
                    .child(consequence(
                        if erase {
                            IconName::Eraser
                        } else {
                            IconName::Trash
                        },
                        "Apaga apps, dados e ajustes do simulador".to_string(),
                        if erase {
                            "o simulador volta ao estado de fábrica".to_string()
                        } else {
                            format!(
                                "libera {} em {devices_dir}",
                                format_bytes(device.disk_bytes)
                            )
                        },
                    ))
                    .child(consequence(
                        IconName::Warning,
                        "Não dá para desfazer".to_string(),
                        if erase {
                            "o modelo e a versão do iOS continuam".to_string()
                        } else {
                            "um simulador novo começa do zero".to_string()
                        },
                    )),
            )
            .when(!erase && runtime_alone, |this| {
                this.child(
                    h_flex()
                        .id("ios-remove-runtime-option")
                        .w_full()
                        .gap_2p5()
                        .p_3()
                        .items_start()
                        .rounded_lg()
                        .bg(colors.editor_background)
                        .border_1()
                        .border_color(colors.border)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Mode::Confirm(Confirmation::Delete { remove_runtime, .. }) =
                                &mut this.mode
                            {
                                *remove_runtime = !*remove_runtime;
                                cx.notify();
                            }
                        }))
                        .child(checkbox_indicator(remove_runtime, cx))
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(
                                    Label::new(format!("Remover também o runtime iOS {version}"))
                                        .size(LabelSize::Small),
                                )
                                .child(
                                    Label::new(format!(
                                        "Nenhum outro simulador usa · libera mais {}",
                                        format_bytes(runtime_bytes)
                                    ))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                        ),
                )
            });
        let footer = modal_footer(cx)
            .child(
                div().flex_1().child(
                    Label::new("esc para cancelar")
                        .size(LabelSize::XSmall)
                        .color(Color::Placeholder),
                ),
            )
            .child(
                secondary_button("ios-confirm-cancel", None, "Cancelar", true, cx).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.mode = Mode::Manage(ManagerTab::Simulators);
                        cx.notify();
                    }),
                ),
            )
            .child(
                destructive_button("ios-confirm-run", action_label, true, cx)
                    .on_click(cx.listener(|this, _, window, cx| this.run_confirmation(window, cx))),
            );
        modal_surface(cx)
            .w(px(520.))
            .child(content)
            .child(footer)
            .into_any_element()
    }

    fn render_wizard(&self, cx: &mut Context<Self>) -> AnyElement {
        let Mode::Wizard(wizard) = &self.mode else {
            return div().into_any_element();
        };
        let title: SharedString = match wizard.purpose {
            WizardPurpose::Create => "Novo simulador iOS".into(),
            WizardPurpose::DownloadRuntime => "Baixar runtime".into(),
        };
        let colors = cx.theme().colors().clone();
        let header = v_flex()
            .w_full()
            .gap_4()
            .pt(px(20.))
            .pr(px(20.))
            .pb_4()
            .pl_6()
            .border_b_1()
            .border_color(colors.border)
            .child(self.render_header(title, None, cx))
            .when(wizard.purpose == WizardPurpose::Create, |this| {
                this.child(self.render_wizard_steps(wizard, cx))
            });
        let body = match wizard.step {
            WizardStep::Model => self.render_model_step(wizard, cx),
            WizardStep::Runtime => self.render_runtime_step(wizard, cx),
            WizardStep::Settings => self.render_settings_step(wizard, cx),
        };
        modal_surface(cx)
            .w(px(760.))
            .child(header)
            .child(body)
            .child(self.render_wizard_footer(wizard, cx))
            .into_any_element()
    }

    fn render_wizard_steps(
        &self,
        wizard: &Wizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors().clone();
        let steps = [
            (WizardStep::Model, "Modelo"),
            (WizardStep::Runtime, "iOS"),
            (WizardStep::Settings, "Ajustes"),
        ];
        let mut row = h_flex().w_full().gap_2p5();
        for (index, (step, label)) in steps.into_iter().enumerate() {
            let done = step < wizard.step;
            let current = step == wizard.step;
            if index > 0 {
                row = row.child(div().flex_1().h_px().bg(if step <= wizard.step {
                    colors.border_focused
                } else {
                    colors.border_selected
                }));
            }
            let success = Color::Success.color(cx);
            row = row.child(
                h_flex()
                    .id(("ios-wizard-step", index))
                    .gap_2()
                    .when(done, |this| {
                        this.cursor_pointer().on_click(
                            cx.listener(move |this, _, _, cx| this.set_wizard_step(step, cx)),
                        )
                    })
                    .child(
                        div()
                            .size(px(20.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .map(|this| {
                                if done {
                                    this.bg(success.opacity(0.12)).child(
                                        Icon::new(IconName::Check)
                                            .size(IconSize::XSmall)
                                            .color(Color::Success),
                                    )
                                } else if current {
                                    this.bg(colors.border_focused).child(
                                        Label::new((index + 1).to_string())
                                            .size(LabelSize::XSmall)
                                            .weight(FontWeight::SEMIBOLD)
                                            .color(Color::Custom(gpui::white())),
                                    )
                                } else {
                                    this.bg(colors.element_hover).child(
                                        Label::new((index + 1).to_string())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Placeholder),
                                    )
                                }
                            }),
                    )
                    .child(
                        Label::new(label)
                            .size(LabelSize::Small)
                            .weight(if current {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .color(if current || done {
                                Color::Default
                            } else {
                                Color::Placeholder
                            }),
                    ),
            );
        }
        row
    }

    fn render_model_step(&self, wizard: &Wizard, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let query = self.search_input.read(cx).text(cx);
        let models: Vec<SimDeviceType> = visible_models(manager, wizard.family, &query)
            .into_iter()
            .cloned()
            .collect();
        let loading = !manager.is_loaded();
        let family = wizard.family;
        let families = segmented_control(cx).w(px(160.)).children(
            ProductFamily::ALL
                .into_iter()
                .enumerate()
                .map(|(index, option)| {
                    segment_option(
                        ("ios-model-family", index),
                        option.label(),
                        option == family,
                        true,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_wizard(cx, |wizard| {
                            if wizard.family != option {
                                wizard.family = option;
                                wizard.device_type = None;
                            }
                        });
                        this.fill_wizard_defaults(cx);
                    }))
                }),
        );
        let selected = wizard.device_type.clone();
        let rows = models
            .chunks(MODEL_COLUMNS)
            .map(|chunk| {
                h_flex()
                    .w_full()
                    .gap_3()
                    .children(chunk.iter().map(|model| {
                        let is_selected = selected.as_deref() == Some(model.identifier.as_str());
                        model_card(model, is_selected, cx).on_click(cx.listener({
                            let identifier = model.identifier.clone();
                            let name = model.name.clone();
                            move |this, _, window, cx| {
                                let previous = match &this.mode {
                                    Mode::Wizard(wizard) => wizard.device_type.clone(),
                                    _ => None,
                                };
                                this.update_wizard(cx, |wizard| {
                                    wizard.device_type = Some(identifier.clone())
                                });
                                this.fill_wizard_defaults(cx);
                                // The name follows the model until the user types their own.
                                let current_name = this.name_input.read(cx).text(cx);
                                let previous_name = previous.and_then(|previous| {
                                    this.manager
                                        .read(cx)
                                        .device_type(&previous)
                                        .map(|model| model.name.clone())
                                });
                                if current_name.is_empty()
                                    || previous_name.as_deref() == Some(current_name.as_str())
                                {
                                    this.name_input
                                        .update(cx, |input, cx| input.set_text(&name, window, cx));
                                }
                            }
                        }))
                    }))
                    .children(
                        (chunk.len()..MODEL_COLUMNS).map(|_| div().flex_1().into_any_element()),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .id("ios-model-step")
            .w_full()
            .max_h(px(520.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .gap_4()
            .pt(px(18.))
            .px_6()
            .pb(px(22.))
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .child(families)
                    .child(div().flex_1())
                    .child(div().w(px(200.)).child(self.search_input.clone())),
            )
            .when(loading && models.is_empty(), |this| {
                this.child(loading_row("Carregando modelos…"))
            })
            .when(!loading && models.is_empty(), |this| {
                this.child(
                    div().py_6().child(
                        Label::new("Nenhum modelo encontrado")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(v_flex().w_full().gap_3().children(rows))
            .into_any_element()
    }

    fn render_runtime_step(&self, wizard: &Wizard, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let model = wizard
            .device_type
            .as_deref()
            .and_then(|identifier| manager.device_type(identifier))
            .cloned();
        let options: Vec<RuntimeOption> = manager
            .runtime_options(model.as_ref())
            .into_iter()
            .filter(|option| option.prerelease == wizard.show_prerelease)
            .collect();
        let loading = manager.is_loading_catalog() || !manager.is_loaded();
        let older_floor = options
            .iter()
            .map(|option| version_key(&option.version)[0])
            .max()
            .map(|newest| newest.saturating_sub(1));
        let (recent, older): (Vec<RuntimeOption>, Vec<RuntimeOption>) =
            if wizard.show_older_runtimes {
                (options, Vec::new())
            } else {
                let mut recent = Vec::new();
                let mut older = Vec::new();
                for option in options {
                    let major = version_key(&option.version)[0];
                    if recent.len() < RECENT_RUNTIME_COUNT
                        && older_floor.is_none_or(|floor| major >= floor)
                    {
                        recent.push(option);
                    } else {
                        older.push(option);
                    }
                }
                (recent, older)
            };
        let older_range = (!older.is_empty()).then(|| {
            let majors: Vec<u32> = older
                .iter()
                .map(|option| version_key(&option.version)[0])
                .collect();
            let newest = majors.iter().max().copied().unwrap_or_default();
            let oldest = majors.iter().min().copied().unwrap_or_default();
            if newest == oldest {
                format!("iOS {oldest}")
            } else {
                format!("iOS {oldest}–{newest}")
            }
        });
        let arch_label = format!(
            "{}{}",
            model
                .as_ref()
                .map(|model| format!("{} · ", model.name))
                .unwrap_or_default(),
            if cfg!(target_arch = "aarch64") {
                "arm64 · Apple Silicon"
            } else {
                "x86_64 · Intel"
            }
        );
        let show_prerelease = wizard.show_prerelease;
        let channel = segmented_control(cx).w(px(180.)).children(
            [(false, "Lançadas"), (true, "Betas")]
                .into_iter()
                .enumerate()
                .map(|(index, (prerelease, label))| {
                    segment_option(
                        ("ios-runtime-channel", index),
                        label,
                        prerelease == show_prerelease,
                        true,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_wizard(cx, |wizard| {
                            if wizard.show_prerelease != prerelease {
                                wizard.show_prerelease = prerelease;
                                wizard.runtime = None;
                            }
                        });
                        this.fill_wizard_defaults(cx);
                    }))
                }),
        );
        let selected = wizard.runtime.as_ref().map(|(_, build)| build.clone());
        let shown = recent.len();
        let rows = recent
            .into_iter()
            .enumerate()
            .map(|(index, option)| {
                let is_selected = selected.as_deref() == Some(option.build.as_str());
                let row = runtime_row(index, &option, is_selected, cx);
                if option.unavailable.is_some() {
                    row
                } else {
                    row.cursor_pointer().on_click(cx.listener({
                        let choice = (option.version, option.build);
                        move |this, _, _, cx| {
                            this.update_wizard(cx, |wizard| wizard.runtime = Some(choice.clone()))
                        }
                    }))
                }
            })
            .collect::<Vec<_>>();
        v_flex()
            .id("ios-runtime-step")
            .w_full()
            .max_h(px(520.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .gap(px(14.))
            .pt(px(18.))
            .px_6()
            .pb(px(22.))
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .child(channel)
                    .child(div().flex_1())
                    .child(
                        Label::new(arch_label)
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    ),
            )
            .when(loading && shown == 0, |this| {
                this.child(loading_row("Carregando versões do iOS…"))
            })
            .when(!loading && shown == 0, |this| {
                this.child(
                    Label::new(if show_prerelease {
                        "Nenhuma beta disponível agora"
                    } else {
                        "Nenhuma versão do iOS disponível"
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .children(rows)
            .when_some(older_range, |this, range| {
                this.child(
                    div()
                        .id("ios-show-older-runtimes")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.update_wizard(cx, |wizard| wizard.show_older_runtimes = true)
                        }))
                        .child(
                            Label::new(format!("Mostrar versões mais antigas ({range})"))
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_settings_step(&self, wizard: &Wizard, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let colors = cx.theme().colors().clone();
        let option = selected_option(manager, wizard);
        let download_bytes = option
            .as_ref()
            .filter(|option| !option.installed)
            .and_then(|option| option.download_bytes);
        let needs_download = option.as_ref().is_some_and(|option| !option.installed);
        let available = manager.available_bytes();
        let short_on_space = self.short_on_space(wizard, cx);
        let model_name = wizard
            .device_type
            .as_deref()
            .and_then(|identifier| manager.device_type(identifier))
            .map(|model| model.name.clone())
            .unwrap_or_default();
        let language = wizard.language;
        let language_menu = self.render_language_menu(
            language,
            |this, option, cx| this.update_wizard(cx, |wizard| wizard.language = option),
            cx,
        );
        let appearance = wizard.appearance;
        let appearance_track = appearance_control(appearance, cx, |this, option, cx| {
            this.update_wizard(cx, |wizard| wizard.appearance = option)
        });
        let boot = wizard.boot_when_ready;
        let options = v_flex()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .overflow_hidden()
            .child(option_row(
                &colors,
                0,
                "Hardware do modelo",
                format!("CPU, memória e tela vêm do {model_name}; o Simulator não deixa ajustar."),
                true,
                true,
                cx,
            ))
            .child(
                option_row(
                    &colors,
                    1,
                    "Iniciar ao terminar",
                    "Abre no dock assim que o simulador estiver criado.".to_string(),
                    boot,
                    false,
                    cx,
                )
                .id("ios-option-boot")
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_wizard(cx, |wizard| {
                        wizard.boot_when_ready = !wizard.boot_when_ready
                    })
                })),
            );

        let accent = colors.border_focused;
        let warning = Color::Warning.color(cx);
        let runtime_label = option
            .as_ref()
            .map(|option| format!("Runtime iOS {} ({})", option.version, option.build))
            .unwrap_or_default();
        let summary = if short_on_space {
            Some(
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_3p5()
                    .rounded_lg()
                    .bg(warning.opacity(0.1))
                    .border_1()
                    .border_color(warning.opacity(0.4))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::XSmall)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new("Espaço insuficiente no disco")
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(
                        Label::new(format!(
                            "O runtime precisa de {} e há {} livres. Libere espaço ou escolha uma versão já instalada.",
                            download_bytes.map(format_bytes).unwrap_or_default(),
                            available.map(format_bytes).unwrap_or_default()
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
        } else if needs_download {
            let mut details = Vec::new();
            if let Some(bytes) = download_bytes {
                details.push(format_bytes(bytes));
            }
            details.push("cada simulador ocupa de 0,5 a 3 GB depois de usado".to_string());
            if let Some(available) = available {
                details.push(format!("{} livres", format_bytes(available)));
            }
            Some(
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_3p5()
                    .rounded_lg()
                    .bg(accent.opacity(0.22))
                    .border_1()
                    .border_color(accent.opacity(0.4))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Download)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new("Antes de criar, vamos baixar")
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(
                        Label::new(runtime_label)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx),
                    )
                    .child(
                        Label::new(details.join(" · "))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
        } else {
            None
        };

        v_flex()
            .id("ios-settings-step")
            .w_full()
            .max_h(px(560.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .gap(px(18.))
            .pt_5()
            .px_6()
            .pb(px(22.))
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .items_end()
                    .child(div().flex_1().child(self.name_input.clone()))
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(field_label("Idioma e região"))
                            .child(language_menu),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .items_end()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(field_label("Aparência"))
                            .child(appearance_track),
                    )
                    .child(div().flex_1()),
            )
            .child(options)
            .children(summary)
            .into_any_element()
    }

    fn render_language_menu(
        &self,
        language: SimLanguage,
        on_pick: fn(&mut Self, SimLanguage, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors().clone();
        let this = cx.weak_entity();
        PopoverMenu::new("ios-language-menu")
            .full_width(true)
            .anchor(Anchor::TopLeft)
            .trigger(ui::PopoverTriggerArea::new(
                h_flex()
                    .id("ios-language-trigger")
                    .w_full()
                    .min_h_8()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .rounded_md()
                    .bg(colors.editor_background)
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(div().flex_1().child(Label::new(language.label)))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            ))
            .menu(move |window, cx| {
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for option in LANGUAGES {
                        let this = this.clone();
                        menu = menu.toggleable_entry(
                            option.label,
                            option == language,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                this.update(cx, |this, cx| on_pick(this, option, cx))
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_wizard_footer(
        &self,
        wizard: &Wizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let manager = self.manager.read(cx);
        let can_advance = self.can_advance(wizard, cx) && !manager.is_busy();
        let option = selected_option(manager, wizard);
        let needs_download = option.as_ref().is_some_and(|option| !option.installed);
        let xcode_version = manager
            .xcode()
            .and_then(|xcode| xcode.version())
            .unwrap_or_default()
            .to_string();
        let hint: SharedString = match wizard.step {
            WizardStep::Model => {
                "Modelos vêm do simctl; CPU, RAM e tela são fixos por modelo.".into()
            }
            WizardStep::Runtime if needs_download => match manager.available_bytes() {
                Some(available) => format!(
                    "O runtime será baixado ao {} · {} livres",
                    if wizard.purpose == WizardPurpose::Create {
                        "criar"
                    } else {
                        "confirmar"
                    },
                    format_bytes(available)
                )
                .into(),
                None => "O runtime será baixado ao criar".into(),
            },
            WizardStep::Runtime => SharedString::default(),
            WizardStep::Settings if needs_download => {
                format!("O download é feito pelo xcodebuild do Xcode {xcode_version}.").into()
            }
            WizardStep::Settings => SharedString::default(),
        };
        let hint_icon = match wizard.step {
            WizardStep::Runtime if needs_download => Some(IconName::Download),
            WizardStep::Settings if needs_download => Some(IconName::Info),
            _ => None,
        };
        let back_label = match (wizard.step, wizard.purpose) {
            (WizardStep::Model, _) | (_, WizardPurpose::DownloadRuntime) => "Cancelar",
            _ => "Voltar",
        };
        let primary_label = match (wizard.step, wizard.purpose) {
            (WizardStep::Runtime, WizardPurpose::DownloadRuntime) => "Baixar",
            (WizardStep::Settings, _) if needs_download => "Baixar e criar",
            (WizardStep::Settings, _) => "Criar",
            _ => "Próximo",
        };
        let primary = if can_advance {
            primary_button("ios-wizard-next", None, primary_label, None, cx)
        } else {
            primary_button("ios-wizard-next", None, primary_label, None, cx)
                .opacity(0.5)
                .cursor_default()
        };
        modal_footer(cx)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .children(hint_icon.map(|icon| {
                        Icon::new(icon)
                            .size(IconSize::XSmall)
                            .color(Color::Placeholder)
                    }))
                    .child(
                        Label::new(hint)
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder)
                            .truncate(),
                    ),
            )
            .child(
                secondary_button("ios-wizard-back", None, back_label, true, cx).on_click(
                    cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                ),
            )
            .child(primary.when(can_advance, |this| {
                this.on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.advance_wizard(window, cx)),
                )
            }))
    }

    fn render_edit(&self, cx: &mut Context<Self>) -> AnyElement {
        let Mode::Edit(form) = &self.mode else {
            return div().into_any_element();
        };
        let manager = self.manager.read(cx);
        let Some(device) = manager.device(&form.udid).cloned() else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors().clone();
        let model_name = manager
            .device_type(&device.device_type_identifier)
            .map(|model| model.name.clone())
            .unwrap_or_else(|| device.name.clone());
        let runtime = manager.runtime(&device.runtime_identifier).cloned();
        let version = device
            .runtime_version()
            .or_else(|| runtime.as_ref().map(|runtime| runtime.version.clone()))
            .map(|version| display_version(&version))
            .unwrap_or_default();
        let header = v_flex()
            .w_full()
            .pt(px(20.))
            .pr(px(20.))
            .pb_4()
            .pl_6()
            .border_b_1()
            .border_color(colors.border)
            .child(self.render_header(format!("Editar {}", device.name).into(), None, cx));
        let language_menu = self.render_language_menu(
            form.language,
            |this, option, cx| this.update_edit(cx, |form| form.language = option),
            cx,
        );
        let appearance_track = appearance_control(form.appearance, cx, |this, option, cx| {
            this.update_edit(cx, |form| form.appearance = option)
        });
        let version_field = h_flex()
            .w_full()
            .min_h_8()
            .px_2()
            .py_1p5()
            .rounded_md()
            .bg(colors.editor_background)
            .border_1()
            .border_color(colors.border_variant)
            .opacity(0.6)
            .child(Label::new(format!("iOS {version} · fixa")).color(Color::Muted));
        let device_type = device.device_type_identifier;
        let info = v_flex()
            .w_full()
            .gap_2()
            .p_3p5()
            .rounded_lg()
            .bg(colors.elevated_surface_background)
            .border_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("A versão do iOS não muda depois de criado")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(
                Label::new(format!(
                    "iOS {version}{}",
                    runtime
                        .as_ref()
                        .map(|runtime| format!(" ({})", runtime.build))
                        .unwrap_or_default()
                ))
                .size(LabelSize::XSmall)
                .buffer_font(cx),
            )
            .child(
                Label::new(format!(
                    "O simctl não troca o runtime de um simulador. Para testar em outra versão, \
                     crie um simulador novo com o mesmo modelo — Duplicar mantém o iOS {version}."
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                h_flex().child(
                    secondary_button(
                        "ios-edit-other-version",
                        None,
                        "Criar com outra versão…",
                        true,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_wizard(
                            WizardPurpose::Create,
                            Some(device_type.clone()),
                            window,
                            cx,
                        )
                    })),
                ),
            );
        let body = v_flex()
            .w_full()
            .gap(px(18.))
            .pt_5()
            .px_6()
            .pb(px(22.))
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .items_end()
                    .child(div().flex_1().child(self.name_input.clone()))
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(field_label("Idioma e região"))
                            .child(language_menu),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .items_end()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(field_label("Aparência"))
                            .child(appearance_track),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(field_label("Versão do iOS"))
                            .child(version_field),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border)
                    .overflow_hidden()
                    .child(option_row(
                        &colors,
                        0,
                        "Hardware do modelo",
                        format!(
                            "CPU, memória e tela vêm do {model_name}; o Simulator não deixa ajustar."
                        ),
                        true,
                        true,
                        cx,
                    )),
            )
            .child(info);
        let can_save = !self.name_input.read(cx).text(cx).trim().is_empty();
        let save = if can_save {
            primary_button("ios-edit-save", None, "Salvar", None, cx)
                .on_click(cx.listener(|this, _, window, cx| this.save_edit(window, cx)))
        } else {
            primary_button("ios-edit-save", None, "Salvar", None, cx)
                .opacity(0.5)
                .cursor_default()
        };
        let footer = modal_footer(cx)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::XSmall)
                            .color(Color::Placeholder),
                    )
                    .child(
                        Label::new(
                            "O idioma vale a partir da próxima inicialização; nada é apagado.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Placeholder)
                        .truncate(),
                    ),
            )
            .child(
                secondary_button("ios-edit-cancel", None, "Cancelar", true, cx).on_click(
                    cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                ),
            )
            .child(save);
        modal_surface(cx)
            .w(px(760.))
            .child(header)
            .child(body)
            .child(footer)
            .into_any_element()
    }
}

impl Render for IosDeviceManagerModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.mode {
            Mode::Manage(tab) => {
                let tab = *tab;
                self.render_manage(tab, cx)
            }
            Mode::Confirm(_) => self.render_confirmation(cx),
            Mode::Wizard(_) => self.render_wizard(cx),
            Mode::Edit(_) => self.render_edit(cx),
        };
        div()
            .key_context("IosDeviceManager")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(content)
    }
}

pub(crate) fn device_status(device: &SimDevice) -> DeviceStatus {
    match device.state {
        _ if !device.is_available => DeviceStatus::RuntimeMissing,
        ios_simulator::SimState::Booted => DeviceStatus::Running,
        ios_simulator::SimState::Booting => DeviceStatus::Booting,
        _ => DeviceStatus::Shutdown,
    }
}

/// Models of `family` matching `query` that can run some runtime this Mac has or can download,
/// newest first, which is the order simctl lists them in.
fn visible_models<'a>(
    manager: &'a IosSimulatorManager,
    family: ProductFamily,
    query: &str,
) -> Vec<&'a SimDeviceType> {
    let query = query.trim().to_lowercase();
    let offered: Vec<RuntimeOption> = manager
        .runtime_options(None)
        .into_iter()
        .filter(|option| option.unavailable.is_none())
        .collect();
    manager
        .device_types()
        .iter()
        .filter(|model| model.family == family)
        .filter(|model| query.is_empty() || model.name.to_lowercase().contains(&query))
        .filter(|model| {
            offered.is_empty() || offered.iter().any(|option| model.supports(&option.version))
        })
        .collect()
}

fn selected_option(manager: &IosSimulatorManager, wizard: &Wizard) -> Option<RuntimeOption> {
    let (_, build) = wizard.runtime.as_ref()?;
    let model = wizard
        .device_type
        .as_deref()
        .and_then(|identifier| manager.device_type(identifier));
    manager
        .runtime_options(model)
        .into_iter()
        .find(|option| &option.build == build)
}

fn appearance_control(
    appearance: Appearance,
    cx: &mut Context<IosDeviceManagerModal>,
    on_pick: fn(&mut IosDeviceManagerModal, Appearance, &mut Context<IosDeviceManagerModal>),
) -> impl IntoElement + use<> {
    segmented_control(cx).w(px(180.)).children(
        [Appearance::Light, Appearance::Dark]
            .into_iter()
            .enumerate()
            .map(|(index, option)| {
                segment_option(
                    ("ios-appearance", index),
                    option.label(),
                    option == appearance,
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| on_pick(this, option, cx)))
            }),
    )
}

fn field_label(label: &'static str) -> Label {
    Label::new(label).size(LabelSize::Small)
}

fn model_card(model: &SimDeviceType, selected: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors().clone();
    const PREVIEW_HEIGHT: f32 = 92.;
    let (width, height) = model
        .screen
        .map_or((1206, 2622), |screen| (screen.width, screen.height));
    let aspect = width as f32 / height.max(1) as f32;
    let (preview_width, preview_height) = if aspect > 1. {
        (PREVIEW_HEIGHT, PREVIEW_HEIGHT / aspect)
    } else {
        (PREVIEW_HEIGHT * aspect, PREVIEW_HEIGHT)
    };
    v_flex()
        .id(gpui::ElementId::Name(
            format!("ios-model-{}", model.identifier).into(),
        ))
        .flex_1()
        .min_w_0()
        .p(px(14.))
        .gap_2p5()
        .items_center()
        .rounded(px(10.))
        .cursor_pointer()
        .map(|this| {
            if selected {
                this.border(px(1.5))
                    .border_color(colors.border_focused)
                    .bg(colors.border_focused.opacity(0.22))
            } else {
                this.border_1()
                    .border_color(colors.border_selected)
                    .hover(|this| this.bg(colors.ghost_element_hover))
            }
        })
        .child(
            div()
                .w_full()
                .h(px(96.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(preview_width))
                        .h(px(preview_height))
                        .rounded(px(8.))
                        .border(px(1.5))
                        .border_color(if selected {
                            colors.text_accent
                        } else {
                            Color::Placeholder.color(cx)
                        }),
                ),
        )
        .child(
            v_flex()
                .w_full()
                .items_center()
                .gap_0p5()
                .child(
                    Label::new(model.name.clone())
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .truncate(),
                )
                .child(
                    Label::new(model.screen_label())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
}

fn runtime_row(
    index: usize,
    option: &RuntimeOption,
    selected: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors().clone();
    let unavailable = option.unavailable.is_some();
    let meta = match &option.unavailable {
        Some(reason) => format!("Build {} · {reason}", option.build),
        None => format!("Build {} · arm64", option.build),
    };
    h_flex()
        .id(("ios-runtime-option", index))
        .w_full()
        .h(px(52.))
        .px(px(14.))
        .gap_3()
        .rounded_lg()
        .when(unavailable, |this| this.opacity(0.45))
        .map(|this| {
            if selected {
                this.border_1()
                    .border_color(colors.border_focused)
                    .bg(colors.border_focused.opacity(0.22))
            } else if unavailable {
                this.border_1().border_color(colors.border)
            } else {
                this.border_1()
                    .border_color(colors.border)
                    .hover(|this| this.bg(colors.ghost_element_hover))
            }
        })
        .child(radio_indicator(selected, cx))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(format!("iOS {}", option.version))
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .when(option.requested, |this| {
                            this.child(
                                div()
                                    .h(px(18.))
                                    .px_1p5()
                                    .flex()
                                    .items_center()
                                    .rounded_sm()
                                    .bg(colors.border_focused.opacity(0.22))
                                    .child(
                                        Label::new("pedido pelo Xcode")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Accent),
                                    ),
                            )
                        }),
                )
                .child(
                    Label::new(meta)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .child(h_flex().gap_1p5().map(|this| {
            if unavailable {
                this.child(
                    Icon::new(IconName::Lock)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new("Indisponível")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            } else if option.installed {
                this.child(
                    Icon::new(IconName::Check)
                        .size(IconSize::XSmall)
                        .color(Color::Success),
                )
                .child(
                    Label::new("Instalado")
                        .size(LabelSize::Small)
                        .color(Color::Success),
                )
            } else {
                let color = if selected {
                    Color::Accent
                } else {
                    Color::Muted
                };
                let label = match option.download_bytes.filter(|bytes| *bytes > 0) {
                    Some(bytes) => format!("Baixar · {}", format_bytes(bytes)),
                    None => "Baixar".to_string(),
                };
                this.child(
                    Icon::new(IconName::Download)
                        .size(IconSize::XSmall)
                        .color(color),
                )
                .child(Label::new(label).size(LabelSize::Small).color(color))
            }
        }))
}

fn option_row(
    colors: &theme::ThemeColors,
    index: usize,
    label: &'static str,
    description: String,
    on: bool,
    locked: bool,
    cx: &App,
) -> gpui::Div {
    h_flex()
        .w_full()
        .px(px(14.))
        .py_2p5()
        .gap_3()
        .when(index > 0, |this| {
            this.border_t_1().border_color(colors.border)
        })
        .child(
            v_flex()
                .flex_1()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(label).size(LabelSize::Small))
                        .when(locked, |this| {
                            this.child(
                                h_flex()
                                    .h(px(18.))
                                    .px_1p5()
                                    .gap_1()
                                    .rounded_sm()
                                    .bg(colors.element_hover)
                                    .child(
                                        Icon::new(IconName::Lock)
                                            .size(IconSize::XSmall)
                                            .color(Color::Placeholder),
                                    )
                                    .child(
                                        Label::new("fixo")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Placeholder),
                                    ),
                            )
                        }),
                )
                .child(
                    Label::new(description)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
        .child(toggle_switch(("ios-option", index), on, !locked, cx))
}

fn loading_row(label: &'static str) -> impl IntoElement {
    h_flex()
        .py_6()
        .gap_2()
        .justify_center()
        .child(
            Icon::new(IconName::ArrowCircle)
                .size(IconSize::Small)
                .color(Color::Muted)
                .with_rotate_animation(2),
        )
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
}

fn plural(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// `~/Library/Developer/CoreSimulator` instead of the full home path.
fn tildify(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    match util::paths::home_dir().to_str() {
        Some(home) if text.starts_with(home) => format!("~{}", &text[home.len()..]),
        _ => text.into_owned(),
    }
}

/// The iOS version a simulator was created with, as the picker shows it.
pub(crate) fn device_os_label(device: &SimDevice) -> Option<String> {
    device
        .runtime_version()
        .or_else(|| runtime_version_from_identifier(&device.runtime_identifier))
        .map(|version| format!("iOS {}", display_version(&version)))
}
