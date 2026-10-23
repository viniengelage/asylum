use crate::{DeviceStatus, render_device_status_chip};
use android_sdk::{
    AndroidAvd, AndroidDeviceProfile, AndroidSdkManager, AndroidSdkState, AvdSettings,
    DEFAULT_RAM_MB, DEFAULT_STORAGE_GB, DeviceCategory, ImageFlavor, Orientation, PrepareRequest,
    SdkComponentKind, SystemImageId, format_bytes, version_key,
};
use gpui::{
    Anchor, AnyElement, ClickEvent, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, ScrollHandle, Subscription, WeakEntity, px,
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

/// How many system images step D lists before "Mostrar versões mais antigas".
const RECENT_IMAGE_COUNT: usize = 5;
const RAM_OPTIONS_MB: [u32; 3] = [2048, 4096, 8192];
const PROFILE_COLUMNS: usize = 4;

/// Where the modal opens.
pub enum ManagerEntry {
    Devices,
    NewDevice,
}

/// The device manager of the Android devices panel: the AVDs and system images (boards B and H),
/// the delete confirmation (G) and the new-device wizard (C, D, E), all in one modal so moving
/// between them never closes it.
pub struct AndroidDeviceManagerModal {
    manager: Entity<AndroidSdkManager>,
    workspace: WeakEntity<Workspace>,
    mode: Mode,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    row_menus: HashMap<String, PopoverMenuHandle<ContextMenu>>,
    name_input: Entity<InputField>,
    storage_input: Entity<InputField>,
    search_input: Entity<InputField>,
    _subscriptions: Vec<Subscription>,
}

enum Mode {
    Manage(ManagerTab),
    Confirm(Confirmation),
    Wizard(Wizard),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagerTab {
    Devices,
    SystemImages,
}

enum Confirmation {
    Delete { avd: String, remove_image: bool },
    Wipe { avd: String },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WizardStep {
    Hardware,
    SystemImage,
    Settings,
}

struct Wizard {
    step: WizardStep,
    purpose: WizardPurpose,
    category: DeviceCategory,
    profile: Option<String>,
    flavor: ImageFlavor,
    image: Option<SystemImageId>,
    show_older_images: bool,
    ram_mb: u32,
    orientation: Orientation,
    hardware_keyboard: bool,
    boot_when_ready: bool,
}

enum WizardPurpose {
    Create,
    Edit {
        avd: String,
        image: Option<SystemImageId>,
    },
    /// "Baixar imagem…" in the system images tab: step D alone.
    DownloadImage,
}

/// A row of step D.
struct ImageOption {
    id: SystemImageId,
    installed: bool,
    download_bytes: Option<u64>,
}

impl EventEmitter<DismissEvent> for AndroidDeviceManagerModal {}
impl ModalView for AndroidDeviceManagerModal {}

impl Focusable for AndroidDeviceManagerModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AndroidDeviceManagerModal {
    pub fn new(
        manager: Entity<AndroidSdkManager>,
        workspace: WeakEntity<Workspace>,
        entry: ManagerEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        manager.update(cx, |manager, cx| {
            manager.refresh_inventory(cx);
            manager.refresh_catalog(cx);
            manager.refresh_device_profiles(cx);
        });
        let name_input = cx.new(|cx| InputField::new(window, cx, "Pixel 9").label("Nome"));
        let storage_input = cx.new(|cx| {
            InputField::new(window, cx, "8")
                .label("Armazenamento interno")
                .end_hint("GB")
        });
        let search_input = cx.new(|cx| {
            InputField::new(window, cx, "Buscar perfil").start_icon(IconName::MagnifyingGlass)
        });
        let mut subscriptions = vec![cx.observe(&manager, |this, _, cx| {
            this.fill_wizard_defaults(cx);
            cx.notify();
        })];
        for input in [&name_input, &storage_input, &search_input] {
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
            mode: Mode::Manage(ManagerTab::Devices),
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            row_menus: HashMap::new(),
            name_input,
            storage_input,
            search_input,
            _subscriptions: subscriptions,
        };
        if matches!(entry, ManagerEntry::NewDevice) {
            this.start_wizard(WizardPurpose::Create, window, cx);
        }
        this
    }

    fn start_wizard(
        &mut self,
        purpose: WizardPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut wizard = Wizard {
            step: WizardStep::Hardware,
            purpose,
            category: DeviceCategory::Phone,
            profile: None,
            flavor: ImageFlavor::GooglePlay,
            image: None,
            show_older_images: false,
            ram_mb: DEFAULT_RAM_MB,
            orientation: Orientation::Portrait,
            hardware_keyboard: true,
            boot_when_ready: true,
        };
        let mut name = String::new();
        let mut storage_gb = DEFAULT_STORAGE_GB;
        match &wizard.purpose {
            WizardPurpose::Create => {}
            WizardPurpose::DownloadImage => wizard.step = WizardStep::SystemImage,
            WizardPurpose::Edit { avd, .. } => {
                let manager = self.manager.read(cx);
                if let Some(avd) = manager.avd(avd) {
                    wizard.step = WizardStep::Settings;
                    wizard.profile = avd.device_profile.clone();
                    if let Some(profile) = avd.device_profile.as_ref().and_then(|id| {
                        manager
                            .device_profiles()
                            .iter()
                            .find(|profile| &profile.id == id)
                    }) {
                        wizard.category = profile.category;
                    }
                    if let Some(image) = &avd.system_image {
                        wizard.flavor = image.flavor().unwrap_or(ImageFlavor::GooglePlay);
                        wizard.image = Some(image.clone());
                    }
                    wizard.ram_mb = avd
                        .ram_mb
                        .map(|ram| {
                            RAM_OPTIONS_MB
                                .into_iter()
                                .min_by_key(|option| option.abs_diff(ram))
                                .unwrap_or(DEFAULT_RAM_MB)
                        })
                        .unwrap_or(DEFAULT_RAM_MB);
                    wizard.orientation = avd.orientation;
                    wizard.hardware_keyboard = avd.hardware_keyboard;
                    name = avd.label();
                    if let Some(bytes) = avd.storage_bytes {
                        storage_gb = (bytes / (1024 * 1024 * 1024)).max(1) as u32;
                    }
                }
            }
        }
        self.name_input
            .update(cx, |input, cx| input.set_text(&name, window, cx));
        self.storage_input.update(cx, |input, cx| {
            input.set_text(&storage_gb.to_string(), window, cx)
        });
        self.search_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.mode = Mode::Wizard(wizard);
        self.fill_wizard_defaults(cx);
        cx.notify();
    }

    /// Picks the profile and image the wizard starts on once the lists have loaded, without
    /// overriding a choice already made.
    fn fill_wizard_defaults(&mut self, cx: &mut Context<Self>) {
        let Mode::Wizard(wizard) = &mut self.mode else {
            return;
        };
        let manager = self.manager.read(cx);
        if wizard.profile.is_none() && !matches!(wizard.purpose, WizardPurpose::DownloadImage) {
            let profiles = sorted_profiles(manager.device_profiles(), wizard.category, "");
            wizard.profile = profiles
                .iter()
                .find(|profile| profile.id == "pixel_9")
                .or(profiles.first())
                .map(|profile| profile.id.clone());
        }
        if wizard.image.is_none() {
            wizard.image = image_options(manager, wizard.flavor)
                .into_iter()
                .find(|option| option.installed)
                .or_else(|| image_options(manager, wizard.flavor).into_iter().next())
                .map(|option| option.id);
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        match &mut self.mode {
            Mode::Manage(_) => self.dismiss(cx),
            Mode::Confirm(_) => self.mode = Mode::Manage(ManagerTab::Devices),
            Mode::Wizard(wizard) => match (wizard.step, &wizard.purpose) {
                (WizardStep::Settings, WizardPurpose::Create) => {
                    wizard.step = WizardStep::SystemImage
                }
                (WizardStep::SystemImage, WizardPurpose::Create) => {
                    wizard.step = WizardStep::Hardware
                }
                (WizardStep::SystemImage, WizardPurpose::Edit { .. }) => {
                    wizard.step = WizardStep::Settings
                }
                (_, WizardPurpose::DownloadImage) => {
                    self.mode = Mode::Manage(ManagerTab::SystemImages)
                }
                _ => self.mode = Mode::Manage(ManagerTab::Devices),
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
            Confirmation::Delete { avd, remove_image } => {
                let (avd, remove_image) = (avd.clone(), *remove_image);
                self.manager
                    .update(cx, |manager, cx| manager.delete_avd(avd, remove_image, cx))
            }
            Confirmation::Wipe { avd } => {
                let avd = avd.clone();
                self.manager
                    .update(cx, |manager, cx| manager.wipe_avd_data(avd, cx))
            }
        };
        self.notify_err(task, window, cx);
        self.mode = Mode::Manage(ManagerTab::Devices);
        cx.notify();
    }

    fn wizard_settings(&self, wizard: &Wizard, cx: &App) -> Option<AvdSettings> {
        let display_name = self.name_input.read(cx).text(cx).trim().to_string();
        let storage_gb = self
            .storage_input
            .read(cx)
            .text(cx)
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|gigabytes| (1..=512).contains(gigabytes))?;
        if display_name.is_empty() {
            return None;
        }
        Some(AvdSettings {
            display_name,
            device_profile: wizard.profile.clone()?,
            image: wizard.image.clone()?,
            ram_mb: wizard.ram_mb,
            storage_gb,
            orientation: wizard.orientation,
            hardware_keyboard: wizard.hardware_keyboard,
        })
    }

    fn can_advance(&self, wizard: &Wizard, cx: &App) -> bool {
        match wizard.step {
            WizardStep::Hardware => wizard.profile.is_some(),
            WizardStep::SystemImage => wizard.image.is_some(),
            WizardStep::Settings => self.wizard_settings(wizard, cx).is_some(),
        }
    }

    fn advance_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Wizard(wizard) = &self.mode else {
            return;
        };
        if !self.can_advance(wizard, cx) {
            return;
        }
        let request = match (wizard.step, &wizard.purpose) {
            (WizardStep::Hardware, _) => {
                let profile_name = wizard.profile.as_ref().and_then(|id| {
                    self.manager
                        .read(cx)
                        .device_profiles()
                        .iter()
                        .find(|profile| &profile.id == id)
                        .map(|profile| profile.name.clone())
                });
                if let Some(name) = profile_name
                    && self.name_input.read(cx).is_empty(cx)
                {
                    self.name_input
                        .update(cx, |input, cx| input.set_text(&name, window, cx));
                }
                self.set_wizard_step(WizardStep::SystemImage, cx);
                return;
            }
            (WizardStep::SystemImage, WizardPurpose::DownloadImage) => {
                let Some(image) = wizard.image.clone() else {
                    return;
                };
                PrepareRequest::DownloadImage(image)
            }
            (WizardStep::SystemImage, _) => {
                self.set_wizard_step(WizardStep::Settings, cx);
                return;
            }
            (WizardStep::Settings, purpose) => {
                let Some(settings) = self.wizard_settings(wizard, cx) else {
                    return;
                };
                match purpose {
                    WizardPurpose::Edit { avd, .. } => PrepareRequest::Edit {
                        avd: avd.clone(),
                        settings,
                    },
                    _ => PrepareRequest::Create {
                        settings,
                        boot_when_ready: wizard.boot_when_ready,
                    },
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

    fn row_menu_handle(&mut self, avd: &str) -> PopoverMenuHandle<ContextMenu> {
        self.row_menus.entry(avd.to_string()).or_default().clone()
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
                IconButton::new("android-manager-close", IconName::Close)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .aria_label("Fechar")
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
            )
    }

    fn render_manage(&mut self, tab: ManagerTab, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let avd_count = manager.avds().len();
        let image_count = manager.inventory().installed_images.len();
        let subtitle = format!(
            "{} · {}",
            plural(avd_count, "dispositivo virtual", "dispositivos virtuais"),
            plural(
                image_count,
                "imagem do sistema instalada",
                "imagens do sistema instaladas"
            )
        );
        let tabs = segmented_control(cx).w(px(300.)).children(
            [
                (ManagerTab::Devices, "Dispositivos"),
                (ManagerTab::SystemImages, "Imagens do sistema"),
            ]
            .into_iter()
            .map(|(option, label)| {
                segment_option(
                    ("android-manager-tab", option as usize),
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
            .child(self.render_header("Dispositivos Android".into(), Some(subtitle.into()), cx))
            .child(tabs);
        let body = match tab {
            ManagerTab::Devices => self.render_devices_tab(cx),
            ManagerTab::SystemImages => self.render_images_tab(cx),
        };
        modal_surface(cx)
            .w(px(700.))
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_devices_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let avds = manager.avds().to_vec();
        let running = manager.running_avd().map(str::to_string);
        let booting = matches!(manager.state(), AndroidSdkState::EmulatorBooting);
        let busy = manager.is_busy();
        let inventory = manager.inventory();
        let footer_text = format!(
            "{} · {} em uso{}",
            tildify(&manager.android_dir()),
            format_bytes(inventory.used_bytes),
            inventory
                .available_bytes
                .map(|available| format!(" · {} livres", format_bytes(available)))
                .unwrap_or_default()
        );
        let android_dir = manager.android_dir();
        let colors = cx.theme().colors().clone();

        let list = v_flex()
            .id("android-avd-list")
            .w_full()
            .max_h(px(420.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .gap_0p5()
            .pt_1()
            .px_3()
            .pb_3()
            .when(avds.is_empty(), |this| {
                this.child(
                    v_flex()
                        .py_8()
                        .items_center()
                        .gap_1()
                        .child(Label::new("Nenhum dispositivo virtual ainda"))
                        .child(
                            Label::new("Crie um para rodar apps Android no dock.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .children(avds.iter().enumerate().map(|(index, avd)| {
                let status = if running.as_deref() == Some(avd.name.as_str()) {
                    if booting {
                        DeviceStatus::Booting
                    } else {
                        DeviceStatus::Running
                    }
                } else if avd.image_missing() {
                    DeviceStatus::ImageMissing
                } else {
                    DeviceStatus::Shutdown
                };
                let menu_handle = self.row_menu_handle(&avd.name);
                self.render_avd_row(index, avd, status, busy, menu_handle, cx)
            }));

        let footer =
            modal_footer(cx)
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
                    secondary_button("android-open-folder", None, "Abrir pasta", true, cx)
                        .on_click(move |_, _, cx| {
                            cx.open_with_system(&android_dir);
                        }),
                )
                .child(
                    primary_button("android-new-device", None, "Novo dispositivo", None, cx)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_wizard(WizardPurpose::Create, window, cx)
                        })),
                );
        v_flex()
            .w_full()
            .border_color(colors.border)
            .child(list)
            .child(footer)
            .into_any_element()
    }

    fn render_avd_row(
        &self,
        index: usize,
        avd: &AndroidAvd,
        status: DeviceStatus,
        busy: bool,
        menu_handle: PopoverMenuHandle<ContextMenu>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors().clone();
        let menu_open = menu_handle.is_deployed();
        let is_running = matches!(status, DeviceStatus::Running | DeviceStatus::Booting);
        let image_missing = matches!(status, DeviceStatus::ImageMissing);
        let mut meta = avd
            .os_label()
            .unwrap_or_else(|| "Imagem desconhecida".to_string());
        if let Some(flavor) = avd.system_image.as_ref().and_then(SystemImageId::flavor) {
            meta.push_str(" · ");
            meta.push_str(flavor.label());
        }
        meta.push_str(" · ");
        if image_missing {
            meta.push_str("imagem não baixada");
        } else {
            meta.push_str(&format_bytes(avd.disk_bytes));
        }

        let name = avd.name.clone();
        let (action_icon, action_color, action_label) = if is_running {
            (IconName::Stop, Color::Error, "Parar")
        } else if image_missing {
            (IconName::Download, Color::Default, "Baixar a imagem")
        } else {
            (IconName::PlayFilled, Color::Success, "Iniciar")
        };
        let action = IconButton::new(("android-avd-action", index), action_icon)
            .icon_size(IconSize::Small)
            .icon_color(action_color)
            .aria_label(action_label)
            .tooltip(Tooltip::text(action_label))
            .disabled(busy && !is_running)
            .on_click(cx.listener({
                let name = name.clone();
                let image = avd.system_image.clone();
                move |this, _, _, cx| {
                    let name = name.clone();
                    if is_running {
                        this.manager
                            .update(cx, |manager, cx| manager.stop_emulator(cx));
                    } else if image_missing {
                        if let Some(image) = image.clone() {
                            this.manager.update(cx, |manager, cx| {
                                manager.prepare(PrepareRequest::DownloadImage(image), cx)
                            });
                            this.dismiss(cx);
                        }
                    } else {
                        this.manager
                            .update(cx, |manager, cx| manager.boot_avd(name, false, cx));
                    }
                }
            }));

        let this = cx.weak_entity();
        let image = avd.system_image.clone();
        let label = avd.label();
        let more = PopoverMenu::new(("android-avd-menu", index))
            .with_handle(menu_handle)
            .anchor(Anchor::TopRight)
            .trigger(
                IconButton::new(("android-avd-more", index), IconName::Ellipsis)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .aria_label(format!("Ações de {label}")),
            )
            .menu(move |window, cx| {
                let this = this.clone();
                let name = name.clone();
                let image = image.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let disk_actions_disabled = is_running || busy;
                    let item = |label: &'static str,
                                icon: IconName,
                                disabled: bool,
                                action: fn(
                        &mut AndroidDeviceManagerModal,
                        String,
                        &mut Window,
                        &mut Context<AndroidDeviceManagerModal>,
                    )| {
                        let this = this.clone();
                        let name = name.clone();
                        ContextMenuEntry::new(label)
                            .icon(icon)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .disabled(disabled)
                            .handler(move |window, cx| {
                                let name = name.clone();
                                this.update(cx, |this, cx| action(this, name, window, cx))
                                    .log_err();
                            })
                    };
                    let delete = {
                        let this = this.clone();
                        let name = name.clone();
                        let image = image.clone();
                        move |window: &mut Window, cx: &mut App| {
                            let name = name.clone();
                            let image = image.clone();
                            this.update(cx, |this, cx| {
                                let remove_image = image.is_some_and(|image| {
                                    !this.manager.read(cx).image_used_by_other_avd(&image, &name)
                                });
                                this.mode = Mode::Confirm(Confirmation::Delete {
                                    avd: name,
                                    remove_image,
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
                        is_running || image_missing,
                        |this, name, _, cx| {
                            this.manager
                                .update(cx, |manager, cx| manager.boot_avd(name, false, cx))
                        },
                    ))
                    .item(item(
                        "Iniciar do zero (cold boot)",
                        IconName::RotateCcw,
                        is_running || image_missing,
                        |this, name, _, cx| {
                            this.manager
                                .update(cx, |manager, cx| manager.boot_avd(name, true, cx))
                        },
                    ))
                    .item(item(
                        "Editar…",
                        IconName::Pencil,
                        disk_actions_disabled,
                        |this, name, window, cx| {
                            let image = this
                                .manager
                                .read(cx)
                                .avd(&name)
                                .and_then(|avd| avd.system_image.clone());
                            this.start_wizard(WizardPurpose::Edit { avd: name, image }, window, cx)
                        },
                    ))
                    .item(item(
                        "Duplicar",
                        IconName::Copy,
                        disk_actions_disabled,
                        |this, name, window, cx| {
                            let task = this
                                .manager
                                .update(cx, |manager, cx| manager.duplicate_avd(name, cx));
                            this.notify_err(task, window, cx);
                        },
                    ))
                    .item(item(
                        "Apagar dados…",
                        IconName::Eraser,
                        disk_actions_disabled,
                        |this, name, window, cx| {
                            this.mode = Mode::Confirm(Confirmation::Wipe { avd: name });
                            window.focus(&this.focus_handle, cx);
                            cx.notify();
                        },
                    ))
                    .item(item(
                        "Mostrar no Finder",
                        IconName::Folder,
                        false,
                        |this, name, _, cx| {
                            let directory = this.manager.read(cx).avd_dir(&name);
                            cx.reveal_path(&directory);
                        },
                    ))
                    .separator()
                    .custom_entry(
                        move |_, _| {
                            h_flex()
                                .gap_2()
                                .opacity(if disk_actions_disabled { 0.5 } else { 1.0 })
                                .child(
                                    Icon::new(IconName::Trash)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                )
                                .child(Label::new("Excluir…").color(Color::Error))
                                .into_any_element()
                        },
                        move |window, cx| {
                            if !disk_actions_disabled {
                                delete(window, cx);
                            }
                        },
                    )
                }))
            });

        h_flex()
            .id(("android-avd-row", index))
            .w_full()
            .h(px(60.))
            .pl(px(14.))
            .pr_3()
            .gap_3()
            .rounded_lg()
            .when(menu_open, |this| this.bg(colors.ghost_element_hover))
            .hover(|this| this.bg(colors.ghost_element_hover))
            .when(is_running && busy, |this| {
                this.tooltip(Tooltip::text("Pare o dispositivo primeiro"))
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
                                Label::new(avd.label())
                                    .weight(FontWeight::SEMIBOLD)
                                    .truncate(),
                            )
                            .child(render_device_status_chip(status, cx)),
                    )
                    .child(
                        Label::new(meta)
                            .size(LabelSize::Small)
                            .color(if image_missing {
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

    fn render_images_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let images = manager.inventory().installed_images.clone();
        let busy = manager.is_busy();
        let components: Vec<(SdkComponentKind, Option<String>, Option<String>)> =
            SdkComponentKind::ALL
                .into_iter()
                .map(|kind| {
                    let installed = manager
                        .inventory()
                        .component_version(kind)
                        .map(str::to_string);
                    let available = manager
                        .catalog()
                        .and_then(|catalog| catalog.component(kind))
                        .map(|component| component.version.clone())
                        .filter(|available| {
                            installed.as_deref().is_none_or(|installed| {
                                version_key(available) > version_key(installed)
                            })
                        });
                    (kind, installed, available)
                })
                .collect();
        let colors = cx.theme().colors().clone();
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

        let image_rows = images
            .iter()
            .enumerate()
            .map(|(index, image)| {
                let unused = image.used_by.is_empty();
                let usage = if unused {
                    "nenhum dispositivo usa".to_string()
                } else {
                    format!("usada por {}", image.used_by.join(", "))
                };
                let id = image.id.clone();
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
                                Label::new(image.id.short_label())
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(format!(
                                    "{} · {}",
                                    flavor_label(&image.id),
                                    image.id.abi
                                ))
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
                    .child(
                        Label::new(format_bytes(image.disk_bytes))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new(("android-remove-image", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .icon_color(if unused {
                                Color::Error
                            } else {
                                Color::Placeholder
                            })
                            .aria_label("Remover imagem")
                            .disabled(!unused || busy)
                            .tooltip(Tooltip::text(if unused {
                                "Remover imagem"
                            } else {
                                "Algum dispositivo usa esta imagem"
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let image = id.clone();
                                let task = this.manager.update(cx, |manager, cx| {
                                    manager.uninstall_system_image(image, cx)
                                });
                                this.notify_err(task, window, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();

        let component_rows = components
            .into_iter()
            .enumerate()
            .map(|(index, (kind, installed, available))| {
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
                            Label::new(kind.label())
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        ),
                    )
                    .child(
                        Label::new(installed.unwrap_or_else(|| "não instalado".to_string()))
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .map(|this| match available {
                        Some(available) => this
                            .child(
                                Label::new(format!("{available} disponível"))
                                    .size(LabelSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(
                                secondary_button(
                                    ("android-update-component", index),
                                    None,
                                    "Atualizar",
                                    !busy,
                                    cx,
                                )
                                .when(!busy, |this| {
                                    this.on_click(cx.listener(move |this, _, _, cx| {
                                        this.manager.update(cx, |manager, cx| {
                                            manager
                                                .prepare(PrepareRequest::UpdateComponent(kind), cx)
                                        });
                                        this.dismiss(cx);
                                    }))
                                }),
                            ),
                        None => this.child(
                            Label::new("atualizado")
                                .size(LabelSize::Small)
                                .color(Color::Placeholder),
                        ),
                    })
            })
            .collect::<Vec<_>>();

        let body = v_flex()
            .id("android-images-body")
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
                    .child(section("IMAGENS INSTALADAS"))
                    .child(
                        table()
                            .when(images.is_empty(), |this| {
                                this.child(
                                    div().p_3().child(
                                        Label::new("Nenhuma imagem instalada")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                                )
                            })
                            .children(image_rows),
                    ),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(section("COMPONENTES DO SDK"))
                    .child(table().children(component_rows)),
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
                                "Imagens só são removidas quando nenhum dispositivo depende delas.",
                            )
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder)
                            .truncate(),
                        ),
                )
                .child(
                    secondary_button("android-download-image", None, "Baixar imagem…", !busy, cx)
                        .when(!busy, |this| {
                            this.on_click(cx.listener(|this, _, window, cx| {
                                this.start_wizard(WizardPurpose::DownloadImage, window, cx)
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
        let (avd_name, wipe) = match confirmation {
            Confirmation::Delete { avd, .. } => (avd, false),
            Confirmation::Wipe { avd } => (avd, true),
        };
        let Some(avd) = manager.avd(avd_name).cloned() else {
            return div().into_any_element();
        };
        let remove_image = matches!(
            confirmation,
            Confirmation::Delete {
                remove_image: true,
                ..
            }
        );
        let image_alone = avd
            .system_image
            .as_ref()
            .is_some_and(|image| !manager.image_used_by_other_avd(image, &avd.name));
        let image_bytes = avd
            .system_image
            .as_ref()
            .and_then(|image| manager.inventory().installed_image(image))
            .map_or(0, |image| image.disk_bytes);
        let error = Color::Error.color(cx);
        let mut subtitle = avd.os_label().unwrap_or_default();
        if let Some(flavor) = avd.system_image.as_ref().and_then(SystemImageId::flavor) {
            subtitle.push_str(" · ");
            subtitle.push_str(flavor.label());
        }
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
        let avd_dir = tildify(&manager.avd_dir(&avd.name));
        let freed = avd.disk_bytes + if remove_image { image_bytes } else { 0 };
        let (title, action_label) = if wipe {
            (
                format!("Apagar os dados de {}?", avd.label()),
                "Apagar dados".to_string(),
            )
        } else {
            (
                format!("Excluir {}?", avd.label()),
                format!("Excluir e liberar {}", format_bytes(freed)),
            )
        };
        let colors = cx.theme().colors().clone();
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
                                Icon::new(if wipe {
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
                        if wipe {
                            IconName::Eraser
                        } else {
                            IconName::Trash
                        },
                        "Apaga apps, contas e arquivos do dispositivo".to_string(),
                        if wipe {
                            "o dispositivo volta ao estado de fábrica no próximo boot".to_string()
                        } else {
                            format!("libera {} em {avd_dir}", format_bytes(avd.disk_bytes))
                        },
                    ))
                    .child(consequence(
                        IconName::Warning,
                        "Não dá para desfazer".to_string(),
                        if wipe {
                            "os ajustes do dispositivo continuam".to_string()
                        } else {
                            "um dispositivo novo começa do zero".to_string()
                        },
                    )),
            )
            .when(!wipe && image_alone, |this| {
                let image_label = avd
                    .system_image
                    .as_ref()
                    .map(|image| format!("{} · {}", image.android_version(), flavor_label(image)))
                    .unwrap_or_default();
                this.child(
                    h_flex()
                        .id("android-remove-image-option")
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
                            if let Mode::Confirm(Confirmation::Delete { remove_image, .. }) =
                                &mut this.mode
                            {
                                *remove_image = !*remove_image;
                                cx.notify();
                            }
                        }))
                        .child(checkbox_indicator(remove_image, cx))
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(
                                    Label::new(format!("Remover também a imagem {image_label}"))
                                        .size(LabelSize::Small),
                                )
                                .child(
                                    Label::new(format!(
                                        "Nenhum outro dispositivo usa · libera mais {}",
                                        format_bytes(image_bytes)
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
                secondary_button("android-confirm-cancel", None, "Cancelar", true, cx).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.mode = Mode::Manage(ManagerTab::Devices);
                        cx.notify();
                    }),
                ),
            )
            .child(
                destructive_button("android-confirm-run", action_label, true, cx)
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
        let title: SharedString = match &wizard.purpose {
            WizardPurpose::Create => "Novo dispositivo Android".into(),
            WizardPurpose::DownloadImage => "Baixar imagem do sistema".into(),
            WizardPurpose::Edit { avd, .. } => format!(
                "Editar {}",
                self.manager
                    .read(cx)
                    .avd(avd)
                    .map(AndroidAvd::label)
                    .unwrap_or_else(|| avd.clone())
            )
            .into(),
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
            .when(
                !matches!(wizard.purpose, WizardPurpose::DownloadImage),
                |this| this.child(self.render_wizard_steps(wizard, cx)),
            );
        let body = match wizard.step {
            WizardStep::Hardware => self.render_hardware_step(wizard, cx),
            WizardStep::SystemImage => self.render_image_step(wizard, cx),
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
        let editing = matches!(wizard.purpose, WizardPurpose::Edit { .. });
        let steps = [
            (WizardStep::Hardware, "Hardware"),
            (WizardStep::SystemImage, "Sistema"),
            (WizardStep::Settings, "Ajustes"),
        ];
        let mut row = h_flex().w_full().gap_2p5();
        for (index, (step, label)) in steps.into_iter().enumerate() {
            let done = step < wizard.step || (editing && step != wizard.step);
            let current = step == wizard.step;
            if index > 0 {
                row = row.child(div().flex_1().h_px().bg(if step <= wizard.step || editing {
                    colors.border_focused
                } else {
                    colors.border_selected
                }));
            }
            // Editing keeps the hardware profile: changing it would need the screen settings
            // `avdmanager` derives from it rewritten too.
            let clickable = !current
                && match &wizard.purpose {
                    WizardPurpose::Create => step < wizard.step,
                    WizardPurpose::Edit { .. } => step != WizardStep::Hardware,
                    WizardPurpose::DownloadImage => false,
                };
            let success = Color::Success.color(cx);
            row = row.child(
                h_flex()
                    .id(("android-wizard-step", index))
                    .gap_2()
                    .when(clickable, |this| {
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

    fn render_hardware_step(&self, wizard: &Wizard, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let query = self.search_input.read(cx).text(cx);
        let profiles: Vec<AndroidDeviceProfile> =
            sorted_profiles(manager.device_profiles(), wizard.category, &query)
                .into_iter()
                .cloned()
                .collect();
        let loading = manager.is_loading_device_profiles();
        let colors = cx.theme().colors().clone();
        let category = wizard.category;
        let categories = segmented_control(cx).w(px(380.)).children(
            DeviceCategory::ALL
                .into_iter()
                .enumerate()
                .map(|(index, option)| {
                    segment_option(
                        ("android-device-category", index),
                        option.label(),
                        option == category,
                        true,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_wizard(cx, |wizard| {
                            if wizard.category != option {
                                wizard.category = option;
                                wizard.profile = None;
                                let flavors = ImageFlavor::for_category(option);
                                if !flavors.contains(&wizard.flavor) {
                                    wizard.flavor = flavors[0];
                                    wizard.image = None;
                                }
                            }
                        });
                        this.fill_wizard_defaults(cx);
                    }))
                }),
        );
        let selected = wizard.profile.clone();
        let rows = profiles
            .chunks(PROFILE_COLUMNS)
            .map(|chunk| {
                h_flex()
                    .w_full()
                    .gap_3()
                    .children(chunk.iter().map(|profile| {
                        let is_selected = selected.as_deref() == Some(profile.id.as_str());
                        let card = hardware_profile_card(profile, is_selected, cx);
                        card.on_click(cx.listener({
                            let id = profile.id.clone();
                            let name = profile.name.clone();
                            move |this, _, window, cx| {
                                let previous = match &this.mode {
                                    Mode::Wizard(wizard) => wizard.profile.clone(),
                                    _ => None,
                                };
                                this.update_wizard(cx, |wizard| wizard.profile = Some(id.clone()));
                                // The name follows the profile until the user types their own.
                                let current_name = this.name_input.read(cx).text(cx);
                                let previous_name = previous.and_then(|previous| {
                                    this.manager
                                        .read(cx)
                                        .device_profiles()
                                        .iter()
                                        .find(|profile| profile.id == previous)
                                        .map(|profile| profile.name.clone())
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
                        (chunk.len()..PROFILE_COLUMNS).map(|_| div().flex_1().into_any_element()),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .id("android-hardware-step")
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
                    .child(categories)
                    .child(div().flex_1())
                    .child(div().w(px(200.)).child(self.search_input.clone())),
            )
            .when(loading && profiles.is_empty(), |this| {
                this.child(loading_row("Carregando perfis de hardware…"))
            })
            .when(!loading && profiles.is_empty(), |this| {
                this.child(
                    div().py_6().child(
                        Label::new("Nenhum perfil nesta categoria")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(v_flex().w_full().gap_3().children(rows))
            .border_color(colors.border)
            .into_any_element()
    }

    fn render_image_step(&self, wizard: &Wizard, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let flavors = match wizard.purpose {
            WizardPurpose::DownloadImage => ImageFlavor::HANDHELD,
            _ => ImageFlavor::for_category(wizard.category),
        };
        let options = image_options(manager, wizard.flavor);
        let loading = manager.is_loading_catalog();
        let newest_api = options
            .iter()
            .filter(|option| option.download_bytes.is_some())
            .filter_map(|option| option.id.api())
            .max();
        let shown = if wizard.show_older_images {
            options.len()
        } else {
            options.len().min(RECENT_IMAGE_COUNT)
        };
        let older_range = options.get(RECENT_IMAGE_COUNT).and_then(|option| {
            let newest_older = option.id.api()?.0;
            let oldest = options.last()?.id.api()?.0;
            Some(format!("API {oldest}–{newest_older}"))
        });
        let abi = match options.first().map(|option| option.id.abi.as_str()) {
            Some("arm64-v8a") | None => "arm64-v8a · Apple Silicon",
            Some(_) => "x86_64 · Intel",
        };
        let flavor = wizard.flavor;
        let selected = wizard.image.clone();
        let flavor_track = segmented_control(cx)
            .w(px(100. * flavors.len() as f32))
            .children(flavors.iter().enumerate().map(|(index, option)| {
                let option = *option;
                segment_option(
                    ("android-image-flavor", index),
                    option.label(),
                    option == flavor,
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.update_wizard(cx, |wizard| {
                        if wizard.flavor != option {
                            wizard.flavor = option;
                            wizard.image = None;
                        }
                    });
                    this.fill_wizard_defaults(cx);
                }))
            }));
        let rows = options
            .into_iter()
            .take(shown)
            .enumerate()
            .map(|(index, option)| {
                let is_selected = selected.as_ref() == Some(&option.id);
                let is_newest = option.download_bytes.is_some() && option.id.api() == newest_api;
                let row = system_image_row(index, &option, is_selected, is_newest, cx);
                row.on_click(cx.listener({
                    let id = option.id;
                    move |this, _, _, cx| {
                        this.update_wizard(cx, |wizard| wizard.image = Some(id.clone()))
                    }
                }))
            })
            .collect::<Vec<_>>();
        v_flex()
            .id("android-image-step")
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
                    .child(flavor_track)
                    .child(div().flex_1())
                    .child(
                        Label::new(abi)
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    ),
            )
            .when(loading && shown == 0, |this| {
                this.child(loading_row("Carregando versões do Android…"))
            })
            .when(!loading && shown == 0, |this| {
                this.child(
                    Label::new("Nenhuma imagem disponível para este tipo")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(rows)
            .when_some(
                older_range.filter(|_| !wizard.show_older_images),
                |this, range| {
                    this.child(
                        div()
                            .id("android-show-older-images")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.update_wizard(cx, |wizard| wizard.show_older_images = true)
                            }))
                            .child(
                                Label::new(format!("Mostrar versões mais antigas ({range})"))
                                    .size(LabelSize::Small)
                                    .color(Color::Accent),
                            ),
                    )
                },
            )
            .into_any_element()
    }

    fn render_settings_step(&self, wizard: &Wizard, cx: &mut Context<Self>) -> AnyElement {
        let manager = self.manager.read(cx);
        let colors = cx.theme().colors().clone();
        let image_installed = wizard
            .image
            .as_ref()
            .is_some_and(|image| manager.inventory().installed_image(image).is_some());
        let download_bytes = wizard.image.as_ref().and_then(|image| {
            manager
                .catalog()
                .and_then(|catalog| catalog.image(image))
                .map(|image| image.download_bytes)
                .filter(|bytes| *bytes > 0)
        });
        let available = manager.inventory().available_bytes;
        let image_changes = match &wizard.purpose {
            WizardPurpose::Edit { image, .. } => image != &wizard.image,
            _ => false,
        };
        let editing_name = match &wizard.purpose {
            WizardPurpose::Edit { avd, .. } => manager.avd(avd).map(AndroidAvd::label),
            _ => None,
        };
        let storage_gb = self
            .storage_input
            .read(cx)
            .text(cx)
            .trim()
            .parse::<u64>()
            .unwrap_or(u64::from(DEFAULT_STORAGE_GB));
        let required = download_bytes.unwrap_or(0)
            + if matches!(wizard.purpose, WizardPurpose::Create) {
                storage_gb * 1024 * 1024 * 1024
            } else {
                0
            };
        let short_on_space = available.is_some_and(|available| available < required);

        let orientation = wizard.orientation;
        let orientation_menu = {
            let this = cx.weak_entity();
            PopoverMenu::new("android-orientation-menu")
                .full_width(true)
                .anchor(Anchor::TopLeft)
                .trigger(ui::PopoverTriggerArea::new(
                    h_flex()
                        .id("android-orientation-trigger")
                        .w_full()
                        .min_h_8()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .rounded_md()
                        .bg(colors.editor_background)
                        .border_1()
                        .border_color(colors.border_variant)
                        .child(div().flex_1().child(Label::new(orientation.label())))
                        .child(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                ))
                .menu(move |window, cx| {
                    let this = this.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for option in [Orientation::Portrait, Orientation::Landscape] {
                            let this = this.clone();
                            menu = menu.toggleable_entry(
                                option.label(),
                                option == orientation,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    this.update(cx, |this, cx| {
                                        this.update_wizard(cx, |wizard| wizard.orientation = option)
                                    })
                                    .log_err();
                                },
                            );
                        }
                        menu
                    }))
                })
        };
        let field_label = |label: &'static str| Label::new(label).size(LabelSize::Small);
        let ram = wizard.ram_mb;
        let ram_track =
            segmented_control(cx)
                .w(px(180.))
                .children(
                    RAM_OPTIONS_MB
                        .into_iter()
                        .enumerate()
                        .map(|(index, option)| {
                            segment_option(
                                ("android-ram", index),
                                format!("{} GB", option / 1024),
                                option == ram,
                                true,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.update_wizard(cx, |wizard| wizard.ram_mb = option)
                                },
                            ))
                        }),
                );
        let play_store = wizard
            .image
            .as_ref()
            .and_then(SystemImageId::flavor)
            .is_some_and(ImageFlavor::has_play_store);
        let keyboard = wizard.hardware_keyboard;
        let boot = wizard.boot_when_ready;
        let creating = matches!(wizard.purpose, WizardPurpose::Create);
        let mut options = v_flex()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .overflow_hidden()
            .child(
                option_row(
                    &colors,
                    0,
                    "Teclado do computador",
                    "Digita direto no emulador (hw.keyboard).",
                    keyboard,
                    false,
                    cx,
                )
                .id("android-option-keyboard")
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_wizard(cx, |wizard| {
                        wizard.hardware_keyboard = !wizard.hardware_keyboard
                    })
                })),
            )
            .child(option_row(
                &colors,
                1,
                "GPU do Mac",
                "Renderiza a ~28 fps; sem ela o Android fica em ~12 fps.",
                true,
                true,
                cx,
            ));
        if creating {
            options = options.child(
                option_row(
                    &colors,
                    2,
                    "Iniciar ao terminar",
                    "Abre no dock assim que o dispositivo estiver criado.",
                    boot,
                    false,
                    cx,
                )
                .id("android-option-boot")
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_wizard(cx, |wizard| {
                        wizard.boot_when_ready = !wizard.boot_when_ready
                    })
                })),
            );
        }
        options = options.child(option_row(
            &colors,
            3,
            "Google Play Store",
            "Vem da imagem escolhida no passo 2.",
            play_store,
            true,
            cx,
        ));

        let accent = colors.border_focused;
        let warning = Color::Warning.color(cx);
        let package = wizard
            .image
            .as_ref()
            .map(SystemImageId::package)
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
                            "São necessários {} e há {} livres. Libere espaço ou escolha menos armazenamento.",
                            format_bytes(required),
                            available.map(format_bytes).unwrap_or_default()
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
        } else if image_changes {
            let previous = match &wizard.purpose {
                WizardPurpose::Edit { image, .. } => image.as_ref().map(SystemImageId::short_label),
                _ => None,
            };
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
                                Label::new(format!(
                                    "Trocar para {} apaga os dados do {}",
                                    wizard
                                        .image
                                        .as_ref()
                                        .map(SystemImageId::android_version)
                                        .unwrap_or_default(),
                                    editing_name.unwrap_or_default()
                                ))
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(Label::new(package).size(LabelSize::XSmall).buffer_font(cx))
                    .child(
                        Label::new(format!(
                            "Apps, contas e arquivos voltam ao estado de fábrica.{}{}",
                            match (image_installed, download_bytes) {
                                (false, Some(bytes)) =>
                                    format!(" A imagem ({}) é baixada antes.", format_bytes(bytes)),
                                (false, None) => " A imagem é baixada antes.".to_string(),
                                (true, _) => String::new(),
                            },
                            previous
                                .map(|previous| format!(" Era {previous}."))
                                .unwrap_or_default()
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
        } else if !image_installed {
            let mut details = Vec::new();
            if let Some(bytes) = download_bytes {
                details.push(format_bytes(bytes));
            }
            if creating {
                details.push(format!(
                    "o dispositivo ocupa até {storage_gb} GB depois de usado"
                ));
            }
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
                                Label::new(if creating {
                                    "Antes de criar, vamos baixar"
                                } else {
                                    "Antes de salvar, vamos baixar"
                                })
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(Label::new(package).size(LabelSize::XSmall).buffer_font(cx))
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
            .id("android-settings-step")
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
                            .child(field_label("Orientação inicial"))
                            .child(orientation_menu),
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
                            .child(field_label("Memória RAM"))
                            .child(ram_track),
                    )
                    .child(div().flex_1().child(self.storage_input.clone())),
            )
            .child(options)
            .children(summary)
            .into_any_element()
    }

    fn render_wizard_footer(
        &self,
        wizard: &Wizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let manager = self.manager.read(cx);
        let can_advance = self.can_advance(wizard, cx) && !manager.is_busy();
        let selected_installed = wizard
            .image
            .as_ref()
            .is_some_and(|image| manager.inventory().installed_image(image).is_some());
        let image_changes = match &wizard.purpose {
            WizardPurpose::Edit { image, .. } => image != &wizard.image,
            _ => false,
        };
        let hint: SharedString = match (wizard.step, &wizard.purpose) {
            (WizardStep::Hardware, _) => "Perfis vêm do avdmanager list device.".into(),
            (WizardStep::SystemImage, _) if !selected_installed && wizard.image.is_some() => {
                match manager.inventory().available_bytes {
                    Some(available) => format!(
                        "A imagem será baixada ao criar · {} livres",
                        format_bytes(available)
                    )
                    .into(),
                    None => "A imagem será baixada ao criar".into(),
                }
            }
            (WizardStep::SystemImage, _) => SharedString::default(),
            (WizardStep::Settings, WizardPurpose::Edit { .. }) if image_changes => {
                "Os outros ajustes não apagam nada.".into()
            }
            (WizardStep::Settings, WizardPurpose::Edit { .. }) => {
                "Os ajustes valem a partir do próximo boot.".into()
            }
            (WizardStep::Settings, _) => {
                "Licenças do Android SDK serão aceitas em seu nome.".into()
            }
        };
        let hint_icon = match wizard.step {
            WizardStep::SystemImage => Some(IconName::Download),
            WizardStep::Settings => Some(IconName::Info),
            WizardStep::Hardware => None,
        };
        let back_label = match (wizard.step, &wizard.purpose) {
            (WizardStep::Hardware, _) | (_, WizardPurpose::DownloadImage) => "Cancelar",
            (WizardStep::Settings, WizardPurpose::Edit { .. }) => "Cancelar",
            _ => "Voltar",
        };
        let primary_label = match (wizard.step, &wizard.purpose) {
            (WizardStep::SystemImage, WizardPurpose::DownloadImage) => "Baixar",
            (WizardStep::Settings, WizardPurpose::Edit { .. }) if image_changes => {
                "Salvar e apagar dados"
            }
            (WizardStep::Settings, WizardPurpose::Edit { .. }) => "Salvar",
            (WizardStep::Settings, _) if !selected_installed => "Baixar e criar",
            (WizardStep::Settings, _) => "Criar",
            _ => "Próximo",
        };
        let primary = if image_changes && wizard.step == WizardStep::Settings {
            destructive_button("android-wizard-next", primary_label, can_advance, cx)
        } else if can_advance {
            primary_button("android-wizard-next", None, primary_label, None, cx)
        } else {
            primary_button("android-wizard-next", None, primary_label, None, cx)
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
                secondary_button("android-wizard-back", None, back_label, true, cx).on_click(
                    cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                ),
            )
            .child(primary.when(can_advance, |this| {
                this.on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.advance_wizard(window, cx)),
                )
            }))
    }
}

impl Render for AndroidDeviceManagerModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.mode {
            Mode::Manage(tab) => {
                let tab = *tab;
                self.render_manage(tab, cx)
            }
            Mode::Confirm(_) => self.render_confirmation(cx),
            Mode::Wizard(_) => self.render_wizard(cx),
        };
        div()
            .key_context("AndroidDeviceManager")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(content)
    }
}

/// Profiles of `category` matching `query`, Pixels newest first, then the generic ones.
fn sorted_profiles<'a>(
    profiles: &'a [AndroidDeviceProfile],
    category: DeviceCategory,
    query: &str,
) -> Vec<&'a AndroidDeviceProfile> {
    let query = query.trim().to_lowercase();
    let mut matching: Vec<(usize, &AndroidDeviceProfile)> = profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| profile.category == category)
        .filter(|(_, profile)| {
            query.is_empty()
                || profile.name.to_lowercase().contains(&query)
                || profile.id.to_lowercase().contains(&query)
        })
        .collect();
    // The definition files list devices oldest first.
    matching.sort_by_key(|(index, profile)| {
        (!profile.id.starts_with("pixel"), std::cmp::Reverse(*index))
    });
    matching.into_iter().map(|(_, profile)| profile).collect()
}

/// The images step D offers for `flavor`: the repository's, plus installed ones it no longer
/// lists, newest first.
fn image_options(manager: &AndroidSdkManager, flavor: ImageFlavor) -> Vec<ImageOption> {
    let inventory = manager.inventory();
    let mut options: Vec<ImageOption> = manager
        .catalog()
        .map(|catalog| catalog.images.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|image| image.id.flavor() == Some(flavor))
        .map(|image| ImageOption {
            installed: inventory.installed_image(&image.id).is_some(),
            download_bytes: Some(image.download_bytes),
            id: image.id.clone(),
        })
        .collect();
    for image in &inventory.installed_images {
        if image.id.flavor() == Some(flavor) && !options.iter().any(|option| option.id == image.id)
        {
            options.push(ImageOption {
                id: image.id.clone(),
                installed: true,
                download_bytes: None,
            });
        }
    }
    options.sort_by_key(|option| std::cmp::Reverse(option.id.api()));
    options
}

fn hardware_profile_card(
    profile: &AndroidDeviceProfile,
    selected: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors().clone();
    const PREVIEW_HEIGHT: f32 = 92.;
    let (width, height) = profile.resolution.unwrap_or((1080, 2400));
    let (width, height) = match profile.category {
        DeviceCategory::Tv => (height, width),
        _ => (width, height),
    };
    let aspect = width as f32 / height.max(1) as f32;
    let (preview_width, preview_height) = if aspect > 1. {
        (PREVIEW_HEIGHT, PREVIEW_HEIGHT / aspect)
    } else {
        (PREVIEW_HEIGHT * aspect, PREVIEW_HEIGHT)
    };
    v_flex()
        .id(gpui::ElementId::Name(
            format!("android-profile-{}", profile.id).into(),
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
                    Label::new(profile.name.clone())
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .truncate(),
                )
                .children(profile.screen_label().map(|label| {
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate()
                })),
        )
}

fn system_image_row(
    index: usize,
    option: &ImageOption,
    selected: bool,
    newest: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors().clone();
    let mut meta = option.id.api_label();
    if let Some(codename) = option.id.codename() {
        meta.push_str(" · ");
        meta.push_str(codename);
    }
    meta.push_str(" · ");
    meta.push_str(&option.id.abi);
    h_flex()
        .id(("android-image-option", index))
        .w_full()
        .h(px(52.))
        .px(px(14.))
        .gap_3()
        .rounded_lg()
        .cursor_pointer()
        .map(|this| {
            if selected {
                this.border_1()
                    .border_color(colors.border_focused)
                    .bg(colors.border_focused.opacity(0.22))
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
                            Label::new(option.id.android_version())
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .when(newest, |this| {
                            this.child(
                                div()
                                    .h(px(18.))
                                    .px_1p5()
                                    .flex()
                                    .items_center()
                                    .rounded_sm()
                                    .bg(colors.border_focused.opacity(0.22))
                                    .child(
                                        Label::new("mais recente")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Accent),
                                    ),
                            )
                        }),
                )
                .child(Label::new(meta).size(LabelSize::XSmall).color(Color::Muted)),
        )
        .child(h_flex().gap_1p5().map(|this| {
            if option.installed {
                this.child(
                    Icon::new(IconName::Check)
                        .size(IconSize::XSmall)
                        .color(Color::Success),
                )
                .child(
                    Label::new("Instalada")
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
    description: &'static str,
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
        .child(toggle_switch(("android-option", index), on, !locked, cx))
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

fn flavor_label(image: &SystemImageId) -> String {
    image
        .flavor()
        .map_or_else(|| image.tag.clone(), |flavor| flavor.label().to_string())
}

fn plural(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// `~/Library/Android/zed` instead of the full home path.
fn tildify(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    match util::paths::home_dir().to_str() {
        Some(home) if text.starts_with(home) => format!("~{}", &text[home.len()..]),
        _ => text.into_owned(),
    }
}
