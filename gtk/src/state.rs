use crate::{dialogs::*, views::*, widgets::*, ActivateEvent, Event, UiEvent};
use firmware_manager::*;

use gtk::prelude::*;
use slotmap::{DefaultKey as Entity, SecondaryMap, SparseSecondaryMap};
use std::{collections::BTreeSet, sync::mpsc::Sender};

/// Manages all state and state interactions with the UI.
pub(crate) struct State {
    /// Components that have been associated with entities.
    pub(crate) components: Components,
    /// All devices will be created as an entity here
    pub(crate) entities: Entities,
    /// If this system has a battery.
    pub(crate) has_battery: bool,
    /// Sends events to the progress signal
    pub(crate) progress_sender: Sender<ActivateEvent>,
    /// A sender to send firmware requests to the background thread
    pub(crate) sender: Sender<FirmwareEvent>,
    /// Events to be processed by the main event loop
    pub(crate) ui_sender: glib::Sender<Event>,
    /// Widgets that will be actively managed.
    pub(crate) widgets: Widgets,
}

/// GTK widgets that are interacted with throughout the lifetime of the firmware widget.
pub(crate) struct Widgets {
    /// Controls the display of error messages.
    pub(crate) info_bar: gtk::InfoBar,
    /// Error messages will be set in this label.
    pub(crate) info_bar_label: gtk::Label,
    /// Controls which view to display in the UI
    pub(crate) stack: gtk::Stack,
    /// The devices view shows a list of all supported devices.
    pub(crate) view_devices: DevicesView,
    /// The empty view is displayed when a scan found no devices.
    pub(crate) view_empty: EmptyView,
}

/// Components are optional pieces of data that are assigned to entities
#[derive(Default)]
pub(crate) struct Components {
    /// The GTK widgets associated with a device are stored here.
    pub(crate) device_widgets: SecondaryMap<Entity, DeviceWidget>,

    /// Tracks progress of a firmware download.
    pub(crate) firmware_download: SecondaryMap<Entity, (u64, u64)>,

    /// The latest version associated with a device, if one exists.
    pub(crate) latest: SecondaryMap<Entity, Box<str>>,

    /// Details about a fwupd device
    #[cfg(feature = "fwupd")]
    pub(crate) fwupd: SparseSecondaryMap<Entity, (FwupdDevice, BTreeSet<FwupdRelease>)>,

    /// Details about system76 system firmware.
    #[cfg(feature = "system76")]
    pub(crate) system76: SparseSecondaryMap<Entity, (System76Digest, System76Changelog)>,

    /// Details about thelio I/O firmware
    #[cfg(feature = "system76")]
    pub(crate) thelio: SparseSecondaryMap<Entity, System76Digest>,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sender: Sender<FirmwareEvent>,
        ui_sender: glib::Sender<Event>,
        progress_sender: Sender<ActivateEvent>,
        stack: gtk::Stack,
        info_bar: gtk::InfoBar,
        info_bar_label: gtk::Label,
        view_devices: DevicesView,
        view_empty: EmptyView,
    ) -> Self {
        let has_battery =
            upower_dbus::UPower::new(-1).and_then(|upower| upower.on_battery()).unwrap_or(false);

        Self {
            entities: Entities::default(),
            components: Components::default(),
            has_battery,
            progress_sender,
            sender,
            widgets: Widgets { info_bar, info_bar_label, stack, view_devices, view_empty },
            ui_sender,
        }
    }

    /// The base method for creating a new firmware device entity.
    pub fn create_device<F: FnOnce(&mut Self, Entity) -> DeviceWidget>(&mut self, func: F) {
        let entity = self.entities.create();
        let widget = func(self, entity);
        self.components.device_widgets.insert(entity, widget);
        self.widgets.stack.show();
        self.widgets.stack.set_visible_child(self.widgets.view_devices.as_ref());
    }

    /// An event that occurs when firmware has successfully updated.
    pub fn device_updated(&mut self, entity: Entity, latest: Box<str>) {
        if let Some(widget) = self.components.device_widgets.get(entity) {
            widget.stack.progress.set_fraction(1.0);
            widget.label.set_text(latest.as_ref());

            let _ =
                self.progress_sender.send(ActivateEvent::Deactivate(widget.stack.progress.clone()));

            if self.entities.is_system(entity) {
                crate::reboot();
            }

            // Wait 1 second before changing the visibility of the stack.
            let sender = self.ui_sender.clone();
            gtk::timeout_add_seconds(1, move || {
                let _ = sender.send(Event::Ui(UiEvent::HideStack(entity)));

                gtk::Continue(false)
            });
        }
    }

    /// An event that occurs when fwupd firmware is found.
    #[cfg(feature = "fwupd")]
    pub fn fwupd(&mut self, signal: FwupdSignal) {
        self.create_device(move |state, entity| {
            let FwupdSignal { info, device, upgradeable, releases } = signal;
            let widget = if device.needs_reboot() {
                state.entities.associate_system(entity);
                state.widgets.view_devices.system(&info)
            } else {
                state.widgets.view_devices.device(&info)
            };

            widget.stack.hide();

            if let Some(latest) = info.latest {
                state.components.latest.insert(entity, latest);
                state.components.fwupd.insert(entity, (device, releases));
                if upgradeable {
                    let sender = state.ui_sender.clone();
                    widget.stack.show();
                    widget.connect_upgrade_clicked(move || {
                        let _ = sender.send(Event::Ui(UiEvent::Update(entity)));
                    });
                }
            }

            let sender = state.ui_sender.clone();
            widget.connect_clicked(move |_| {
                let _ = sender.send(Event::Ui(UiEvent::Reveal(entity)));
            });

            widget
        });
    }

    /// Reveals a widget's changelog in a revealer, and generate that changelog if it has not been
    /// revealed yet.
    pub fn reveal(&mut self, entity: Entity) {
        let widget = &self.components.device_widgets[entity];
        let revealer = &widget.revealer;
        let sender = &self.ui_sender;

        #[cfg(feature = "fwupd")]
        {
            if let Some((_, releases)) = self.components.fwupd.get(entity) {
                reveal(revealer, sender, entity, move || {
                    let releases = &releases;
                    let log_entries = releases
                        .iter()
                        .rev()
                        .map(|release| (release.version.as_ref(), release.description.as_ref()));

                    crate::changelog::generate_widget(log_entries, true).upcast::<gtk::Container>()
                });

                return;
            }
        }

        #[cfg(feature = "system76")]
        {
            if let Some((_, changelog)) = self.components.system76.get(entity) {
                reveal(revealer, &sender, entity, || {
                    let log_entries = changelog.versions.iter().map(|version| {
                        (
                            version.bios.as_ref(),
                            version.description.as_ref().map_or("N/A", |desc| desc.as_ref()),
                        )
                    });

                    crate::changelog::generate_widget(log_entries, true).upcast::<gtk::Container>()
                });

                return;
            }
        }

        reveal(revealer, &sender, entity, || {
            crate::changelog::generate_widget_none().upcast::<gtk::Container>()
        });
    }

    /// An event that occurs when System76 system firmware has been found.
    #[cfg(feature = "system76")]
    pub fn system76_system(
        &mut self,
        info: FirmwareInfo,
        downloaded: Option<(System76Digest, System76Changelog)>,
    ) {
        self.create_device(move |state, entity| {
            let widget = state.widgets.view_devices.system(&info);
            widget.stack.hide();
            state.entities.associate_system(entity);

            if let Some(latest) = info.latest {
                if latest.as_ref() != info.current.as_ref() {
                    widget.stack.show();
                    let sender = state.ui_sender.clone();
                    widget.connect_upgrade_clicked(move || {
                        let _ = sender.send(Event::Ui(UiEvent::Update(entity)));
                    });
                }

                state.components.latest.insert(entity, latest);
                if let Some(data) = downloaded {
                    state.components.system76.insert(entity, data);
                }
            }

            let sender = state.ui_sender.clone();
            widget.connect_clicked(move |_| {
                let _ = sender.send(Event::Ui(UiEvent::Reveal(entity)));
            });

            widget
        });
    }

    /// An event that occurs when a Thelio I/O board was discovered.
    #[cfg(feature = "system76")]
    pub fn thelio_io(&mut self, info: FirmwareInfo, digest: Option<System76Digest>) {
        self.create_device(move |state, entity| {
            let widget = state.widgets.view_devices.device(&info);

            let sender = state.ui_sender.clone();
            let mut upgradeable = false;

            if let (Some(digest), Some(latest)) = (digest, info.latest) {
                upgradeable = info.current.as_ref() != latest.as_ref();
                widget.connect_upgrade_clicked(move || {
                    let _ = sender.send(Event::Ui(UiEvent::Update(entity)));
                });

                state.components.latest.insert(entity, latest);
                state.components.thelio.insert(entity, digest);
            }

            {
                // When the device's widget is clicked.
                let sender = state.ui_sender.clone();
                widget.connect_clicked(move |_| {
                    let _ = sender.send(Event::Ui(UiEvent::Reveal(entity)));
                });
            }

            if upgradeable {
                widget.stack.show();
            } else {
                widget.stack.hide();
            }

            widget
        });
    }

    /// Schedules the given firmware for an update, and show a dialog if it requires a reboot.
    pub fn update(&mut self, entity: Entity) {
        if let Some(latest) = self.components.latest.get(entity) {
            let widgets = &self.components.device_widgets[entity];

            #[cfg(feature = "fwupd")]
            {
                if let Some((device, releases)) = self.components.fwupd.get(entity) {
                    let dialog = FwupdDialog {
                        device: &device,
                        entity,
                        has_battery: self.has_battery,
                        latest: &latest,
                        needs_reboot: self.entities.is_system(entity),
                        releases: &releases,
                        sender: &self.sender,
                        widgets,
                    };

                    dialog.run();

                    return;
                }
            }

            #[cfg(feature = "system76")]
            {
                if let Some((digest, changelog)) = self.components.system76.get(entity) {
                    let dialog = System76Dialog {
                        changelog: &changelog,
                        digest: &digest,
                        entity,
                        has_battery: self.has_battery,
                        latest: &latest,
                        sender: &self.sender,
                        widgets,
                    };

                    dialog.run();
                } else if let Some(digest) = self.components.thelio.get(entity) {
                    // Exchange the button for a progress bar.
                    widgets.stack.switch_to_waiting();
                    let _ = self
                        .progress_sender
                        .send(ActivateEvent::Activate(widgets.stack.progress.clone()));

                    let _ = self.sender.send(FirmwareEvent::ThelioIo(entity, digest.clone()));
                }
            }
        } else {
            eprintln!(
                "attempted to update firmware for a device which did not have updated firmware"
            );
        }
    }
}

fn reveal<F: FnMut() -> gtk::Container>(
    revealer: &gtk::Revealer,
    sender: &glib::Sender<Event>,
    entity: Entity,
    mut func: F,
) {
    let reveal = if revealer.get_reveal_child() {
        false
    } else {
        // If the content to be revealed has not been generated yet, do so.
        if revealer.get_child().is_none() {
            let widget = func();

            let container = cascade! {
                gtk::Box::new(gtk::Orientation::Vertical, 12);
                ..set_vexpand(true);
                ..add(&gtk::Separator::new(gtk::Orientation::Horizontal));
                ..add(&gtk::LabelBuilder::new().label("<b>Changelog</b>").use_markup(true).xalign(0.0).build());
                ..add(&widget);
                ..show_all();
            };

            revealer.add(&container);
            revealer.show_all();
        }

        true
    };

    let _ = sender.send(Event::Ui(UiEvent::Revealed(entity, reveal)));
    revealer.set_reveal_child(reveal);
}
