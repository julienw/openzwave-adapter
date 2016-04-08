extern crate openzwave_stateful as openzwave;
extern crate foxbox_taxonomy as taxonomy;
extern crate transformable_channels;
#[macro_use]
extern crate log;

use taxonomy::util::Id as TaxId;
use taxonomy::services::{ Setter, Getter, AdapterId, ServiceId, Service, Channel, ChannelKind };
use taxonomy::values::*;
use taxonomy::api::{ ResultMap, Error as TaxError, InternalError, User };
use taxonomy::adapter::{ AdapterManagerHandle, AdapterWatchGuard, WatchEvent };
use transformable_channels::mpsc::ExtSender;

use openzwave::{ ConfigPath, InitOptions, ZWaveManager, ZWaveNotification };
use openzwave::{ CommandClass, ValueGenre, ValueType, ValueID };
use openzwave::{ Controller };

use std::error;
use std::fmt;
use std::thread;
use std::sync::mpsc;
use std::sync::{ Arc, Mutex, RwLock, Weak };
use std::collections::{ HashMap, HashSet };

pub use self::OpenzwaveAdapter as Adapter;

#[derive(Debug)]
pub enum Error {
    TaxonomyError(TaxError),
    OpenzwaveError(openzwave::Error),
    UnknownError
}

impl From<TaxError> for Error {
    fn from(err: TaxError) -> Self {
        Error::TaxonomyError(err)
    }
}

impl From<()> for Error {
    fn from(_: ()) -> Self {
        Error::UnknownError
    }
}

impl From<openzwave::Error> for Error {
    fn from(error: openzwave::Error) -> Self {
        Error::OpenzwaveError(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::TaxonomyError(ref err)  => write!(f, "{}: {}", error::Error::description(self), err),
            Error::OpenzwaveError(ref err) => write!(f, "{}: {}", error::Error::description(self), err),
            Error::UnknownError => write!(f, "{}", error::Error::description(self)),
        }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::TaxonomyError(_) => "Taxonomy Error",
            Error::OpenzwaveError(_) => "Openzwave Error",
            Error::UnknownError => "Unknown error",
        }
    }

    fn cause(&self) -> Option<&error::Error> {
        match *self {
            Error::TaxonomyError(ref err) => Some(err),
            Error::OpenzwaveError(ref err) => Some(err),
            Error::UnknownError => None,
        }
    }
}

#[derive(Debug, Clone)]
struct IdMap<Kind, Type> {
    map: Arc<RwLock<Vec<(TaxId<Kind>, Type)>>>
}

impl<Kind, Type> IdMap<Kind, Type> where Type: Eq + Clone, Kind: Clone {
    fn new() -> Self {
        IdMap {
            map: Arc::new(RwLock::new(Vec::new()))
        }
    }

    fn push(&mut self, id: TaxId<Kind>, ozw_object: Type) -> Result<(), ()> {
        let mut guard = try!(self.map.write().or(Err(())));
        guard.push((id, ozw_object));
        Ok(())
    }

    fn find_tax_id_from_ozw(&self, needle: &Type) -> Result<Option<TaxId<Kind>>, ()> {
        let guard = try!(self.map.read().or(Err(())));
        let find_result = guard.iter().find(|&&(_, ref controller)| controller == needle);
        Ok(find_result.map(|&(ref id, _)| id.clone()))
    }

    fn find_ozw_from_tax_id(&self, needle: &TaxId<Kind>) -> Result<Option<Type>, ()> {
        let guard = try!(self.map.read().or(Err(())));
        let find_result = guard.iter().find(|&&(ref id, _)| id == needle);
        Ok(find_result.map(|&(_, ref ozw_object)| ozw_object.clone()))
    }
}

type SyncExtSender = Mutex<Box<ExtSender<WatchEvent>>>;
type WatchersMap = HashMap<usize, Arc<SyncExtSender>>;
struct Watchers {
    current_index: usize,
    map: Arc<Mutex<WatchersMap>>,
    getter_map: HashMap<TaxId<Getter>, Vec<Weak<SyncExtSender>>>,
}

impl Watchers {
    fn new() -> Self {
        Watchers {
            current_index: 0,
            map: Arc::new(Mutex::new(HashMap::new())),
            getter_map: HashMap::new(),
        }
    }

    fn push(&mut self, tax_id: TaxId<Getter>, watcher: Arc<SyncExtSender>) -> WatcherGuard {
        let index = self.current_index;
        self.current_index += 1;
        {
            let mut map = self.map.lock().unwrap();
            map.insert(index, watcher.clone());
        }

        let entry = self.getter_map.entry(tax_id).or_insert(Vec::new());
        entry.push(Arc::downgrade(&watcher));

        WatcherGuard {
            key: index,
            map: self.map.clone()
        }
    }

    fn get(&self, index: usize) -> Option<Arc<SyncExtSender>> {
        let map = self.map.lock().unwrap();
        map.get(&index).cloned()
    }

    fn get_from_tax_id(&self, tax_id: &TaxId<Getter>) -> Option<Vec<Arc<SyncExtSender>>> {
        self.getter_map.get(tax_id).and_then(|vec| {
            let vec: Vec<_> = vec.iter().filter_map(|weak_sender| weak_sender.upgrade()).collect();
            if vec.len() == 0 { None } else { Some(vec) }
        })
    }
}

fn kind_from_value(value: ValueID) -> Option<ChannelKind> {
    value.get_command_class().map(|cc| match cc {
        CommandClass::SensorBinary => ChannelKind::OpenClosed,
        _ => ChannelKind::Ready // TODO
    })
}

fn to_open_closed(value: &ValueID) -> Option<Value> {
    debug_assert_eq!(value.get_type(), ValueType::ValueType_Bool);

    value.as_bool().ok().map(|val| {
        Value::OpenClosed(
            if val { OpenClosed::Open } else { OpenClosed::Closed }
        )
    })
}

struct WatcherGuard {
    key: usize,
    map: Arc<Mutex<WatchersMap>>,
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap();
        map.remove(&self.key);
    }
}

impl AdapterWatchGuard for WatcherGuard {}

pub struct OpenzwaveAdapter {
    id: TaxId<AdapterId>,
    name: String,
    vendor: String,
    version: [u32; 4],
    ozw: ZWaveManager,
    controller_map: IdMap<ServiceId, Controller>,
    getter_map: IdMap<Getter, ValueID>,
    setter_map: IdMap<Setter, ValueID>,
    watchers: Arc<Mutex<Watchers>>,
}

impl OpenzwaveAdapter {
    pub fn init<T: AdapterManagerHandle + Send + Sync + 'static> (box_manager: &Arc<T>) -> Result<(), Error> {
        let options = InitOptions {
            device: None, // TODO we should expose this as a Value
            config_path: ConfigPath::Default,
            user_path: "./config/openzwave/",
        };

        let (ozw, rx) = try!(match openzwave::init(&options) {
            Err(openzwave::Error::NoDeviceFound) => {
                // early return: we should not impair foxbox startup for this error.
                // TODO concept of FatalError vs IgnoreableError
                error!("No ZWave device has been found.");
                return Ok(());
            }
            result => result
        });

        let name = String::from("OpenZwave Adapter");
        let adapter = Arc::new(OpenzwaveAdapter {
            id: TaxId::new(&name),
            name: name,
            vendor: String::from("Mozilla"),
            version: [1, 0, 0, 0],
            ozw: ozw,
            controller_map: IdMap::new(),
            getter_map: IdMap::new(),
            setter_map: IdMap::new(),
            watchers: Arc::new(Mutex::new(Watchers::new())),
        });

        adapter.spawn_notification_thread(rx, box_manager);
        try!(box_manager.add_adapter(adapter));

        info!("Started Openzwave adapter.");

        Ok(())
    }

    fn spawn_notification_thread<T: AdapterManagerHandle + Send + Sync + 'static>(&self, rx: mpsc::Receiver<ZWaveNotification>, box_manager: &Arc<T>) {
        let adapter_id = self.id.clone();
        let box_manager = box_manager.clone();
        let mut controller_map = self.controller_map.clone();
        let mut getter_map = self.getter_map.clone();
        let mut setter_map = self.setter_map.clone();
        let watchers = self.watchers.clone();

        thread::spawn(move || {
            for notification in rx {
                match notification {
                    ZWaveNotification::ControllerReady(controller) => {
                        let service = format!("OpenZWave/{}", controller.get_home_id());
                        let service_id = TaxId::new(&service);
                        controller_map.push(service_id.clone(), controller);

                        box_manager.add_service(Service::empty(service_id.clone(), adapter_id.clone()));
                    }
                    ZWaveNotification::NodeNew(node)               => {}
                    ZWaveNotification::NodeAdded(node)             => {}
                    ZWaveNotification::NodeRemoved(node)           => {}
                    ZWaveNotification::ValueAdded(value)           => {
                        if value.get_genre() != ValueGenre::ValueGenre_User { continue }

                        let value_id = format!("OpenZWave/{} ({})", value.get_id(), value.get_label());

                        let controller_id = controller_map.find_tax_id_from_ozw(&value.get_controller()).unwrap();
                        if controller_id.is_none() { continue }
                        let controller_id = controller_id.unwrap();

                        let has_getter = !value.is_write_only();
                        let has_setter = !value.is_read_only();

                        let kind = kind_from_value(value);
                        if kind.is_none() { continue }
                        let kind = kind.unwrap();

                        if has_getter {
                            let getter_id = TaxId::new(&value_id);
                            getter_map.push(getter_id.clone(), value);
                            box_manager.add_getter(Channel {
                                id: getter_id.clone(),
                                service: controller_id.clone(),
                                adapter: adapter_id.clone(),
                                last_seen: None,
                                tags: HashSet::new(),
                                mechanism: Getter {
                                    kind: kind.clone(),
                                    updated: None
                                }
                            });
                        }

                        if has_setter {
                            let setter_id = TaxId::new(&value_id);
                            setter_map.push(setter_id.clone(), value);
                            box_manager.add_setter(Channel {
                                id: setter_id.clone(),
                                service: controller_id.clone(),
                                adapter: adapter_id.clone(),
                                last_seen: None,
                                tags: HashSet::new(),
                                mechanism: Setter {
                                    kind: kind,
                                    updated: None
                                }
                            });
                        }
                    }
                    ZWaveNotification::ValueChanged(value)         => {
                        match value.get_type() {
                            ValueType::ValueType_Bool => {},
                            _ => continue // ignore non-bool vals for now
                        };

                        let tax_id = match getter_map.find_tax_id_from_ozw(&value) {
                            Ok(Some(tax_id)) => tax_id,
                            _ => continue
                        };

                        let watchers = watchers.lock().unwrap();

                        let watchers = match watchers.get_from_tax_id(&tax_id) {
                            Some(watchers) => watchers,
                            _ => continue
                        };

                        for sender in &watchers {
                            let sender = sender.lock().unwrap();
                            if let Some(value) = to_open_closed(&value) {
                                sender.send(
                                    WatchEvent::Enter { id: tax_id.clone(), value: value }
                                );
                            }
                        }
                    }
                    ZWaveNotification::ValueRemoved(value)         => {}
                    ZWaveNotification::Generic(string)             => {}
                    other => { warn!("Notification not handled {:?}", other)}
                }
            }
        });
    }
}

impl taxonomy::adapter::Adapter for OpenzwaveAdapter {
    fn id(&self) -> TaxId<AdapterId> {
        self.id.clone()
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn vendor(&self) -> &str {
        &self.vendor
    }

    fn version(&self) -> &[u32; 4] {
        &self.version
    }

    fn fetch_values(&self, mut set: Vec<TaxId<Getter>>, _: User) -> ResultMap<TaxId<Getter>, Option<Value>, TaxError> {
        set.drain(..).map(|id| {
            let ozw_value: Option<ValueID> = self.getter_map.find_ozw_from_tax_id(&id).unwrap(); //FIXME no unwrap

            let ozw_value: Option<Option<Value>> = ozw_value.map(|ozw_value: ValueID| {
                if !ozw_value.is_set() { return None }

                let result: Option<Value> = match ozw_value.get_type() {
                    ValueType::ValueType_Bool => to_open_closed(&ozw_value),
                    _ => Some(Value::Unit)
                };
                result
            });
            let value_result: Result<Option<Value>, TaxError> = ozw_value.ok_or(TaxError::InternalError(InternalError::NoSuchGetter(id.clone())));
            (id, value_result)
        }).collect()
    }

    fn send_values(&self, values: HashMap<TaxId<Setter>, Value>, _: User) -> ResultMap<TaxId<Setter>, (), TaxError> {
        unimplemented!()
    }

    fn register_watch(&self, mut values: Vec<(TaxId<Getter>, Option<Range>)>, sender: Box<ExtSender<WatchEvent>>) -> ResultMap<TaxId<Getter>, Box<AdapterWatchGuard>, TaxError> {
        let sender = Arc::new(Mutex::new(sender)); // Mutex is necessary because cb is not Sync.
        values.drain(..).map(|(id, _)| {
            let watch_guard = {
                let mut watchers = self.watchers.lock().unwrap();
                watchers.push(id.clone(), sender.clone())
            };
            let value_result: Result<Box<AdapterWatchGuard>, TaxError> = Ok(Box::new(watch_guard));

            // if there is a set value already, let's send it.
            let ozw_value: Option<ValueID> = self.getter_map.find_ozw_from_tax_id(&id).unwrap(); // FIXME no unwrap
            if let Some(value) = ozw_value {
                if value.is_set() && value.get_type() == ValueType::ValueType_Bool {
                    if let Some(value) = to_open_closed(&value) {
                        let sender = sender.lock().unwrap();
                        sender.send(
                            WatchEvent::Enter { id: id.clone(), value: value }
                        );
                    }
                }
            }

            (id, value_result)
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
    }
}

