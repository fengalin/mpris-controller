use crossbeam_channel as channel;
use eframe::{egui, epi};
use std::sync::{Arc, Mutex};

use super::{controller, Dispatcher};
use crate::{midi, mpris};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("MIDI error: {}", .0)]
    Midi(#[from] midi::Error),

    #[error("MPRIS error: {}", .0)]
    Mpris(#[from] mpris::Error),
}

pub enum Request {
    Connect((super::port::Direction, Arc<str>)),
    Disconnect(super::port::Direction),
    RefreshPorts,
    UseControlSurface(Arc<str>),
    UsePlayer(Arc<str>),
    RefreshPlayers,
    ResetPlayer,
    Shutdown,
    HaveFrame(epi::Frame),
    HaveContext(egui::Context),
}

pub struct App {
    req_tx: channel::Sender<Request>,
    err_rx: channel::Receiver<Error>,
    ctrl_surf_widget: Arc<Mutex<super::ControlSurfaceWidget>>,
    ports_widget: Arc<Mutex<super::PortsWidget>>,
    player_widget: Arc<Mutex<super::PlayerWidget>>,
    last_err: Option<Error>,
    controller_thread: Option<std::thread::JoinHandle<()>>,
}

impl App {
    pub fn try_new(client_name: &str) -> Result<Self, Error> {
        let (err_tx, err_rx) = channel::unbounded();
        let (req_tx, req_rx) = channel::unbounded();

        let ctrl_surf_widget = Arc::new(Mutex::new(super::ControlSurfaceWidget::new()));
        let ports_widget = Arc::new(Mutex::new(super::PortsWidget::new()));
        let player_widget = Arc::new(Mutex::new(super::PlayerWidget::new()));

        let controller_thread = controller::Spawner {
            req_rx,
            err_tx,
            ctrl_surf_widget: ctrl_surf_widget.clone(),
            client_name: client_name.into(),
            ports_widget: ports_widget.clone(),
            player_widget: player_widget.clone(),
        }
        .spawn();

        Ok(Self {
            req_tx,
            err_rx,
            ports_widget,
            ctrl_surf_widget,
            player_widget,
            last_err: None,
            controller_thread: Some(controller_thread),
        })
    }
}

impl epi::App for App {
    fn name(&self) -> &str {
        "mpris-controller"
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &epi::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("MPRIS Controller");

            ui.add_space(10f32);

            ui.group(|ui| {
                let resp = self.ctrl_surf_widget.lock().unwrap().show(ui);
                Dispatcher::<super::ControlSurfaceWidget>::handle(self, resp);

                ui.add_space(2f32);

                ui.horizontal(|ui| {
                    use super::port::Direction;

                    let resp_in = self.ports_widget.lock().unwrap().show(Direction::In, ui);
                    ui.add_space(20f32);
                    let resp_out = self.ports_widget.lock().unwrap().show(Direction::Out, ui);

                    Dispatcher::<super::PortsWidget>::handle(self, resp_in.or(resp_out));
                });

                ui.add_space(2f32);
                ui.separator();

                let resp = self.player_widget.lock().unwrap().show(ui);
                Dispatcher::<super::PlayerWidget>::handle(self, resp);
            });

            self.pop_err();
            if let Some(ref err) = self.last_err {
                ui.add_space(5f32);
                let text = egui::RichText::new(err.to_string())
                    .color(egui::Color32::WHITE)
                    .background_color(egui::Color32::DARK_RED);
                ui.group(|ui| {
                    use egui::Widget;
                    let label = egui::Label::new(text).sense(egui::Sense::click());
                    if label.ui(ui).clicked() {
                        self.clear_last_err();
                    }
                });
            }
        });
    }

    fn setup(
        &mut self,
        ctx: &egui::Context,
        frame: &epi::Frame,
        storage: Option<&dyn epi::Storage>,
    ) {
        ctx.set_visuals(egui::Visuals::dark());
        self.req_tx.send(Request::HaveFrame(frame.clone())).unwrap();
        self.req_tx.send(Request::HaveContext(ctx.clone())).unwrap();

        let resps = self.ports_widget.lock().unwrap().setup(storage);
        for resp in resps {
            Dispatcher::<super::PortsWidget>::handle(self, Some(resp));
        }

        self.player_widget.lock().unwrap().setup(storage);

        self.send_req(Request::RefreshPorts);
        self.send_req(Request::RefreshPlayers);
    }

    fn save(&mut self, storage: &mut dyn epi::Storage) {
        log::info!("Saving...");
        self.ports_widget.lock().unwrap().save(storage);
        self.player_widget.lock().unwrap().save(storage);
        self.clear_last_err();
    }

    fn on_exit(&mut self) {
        log::info!("Exiting...");
        self.send_req(Request::ResetPlayer);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        log::info!("Shutting down");
        self.shutdown();
    }
}

impl App {
    pub fn shutdown(&mut self) {
        if let Some(controller_thread) = self.controller_thread.take() {
            if let Err(err) = self.req_tx.send(Request::Shutdown) {
                log::error!("App couldn't request shutdown: {}", err);
            } else {
                let _ = controller_thread.join();
            }
        }
    }

    pub fn send_req(&mut self, req: Request) {
        self.req_tx.send(req).unwrap();
    }

    pub fn clear_last_err(&mut self) {
        self.last_err = None;
    }

    fn pop_err(&mut self) {
        match self.err_rx.try_recv() {
            Err(channel::TryRecvError::Empty) => (),
            Ok(err) => self.last_err = Some(err),
            Err(err) => panic!("{}", err),
        }
    }
}