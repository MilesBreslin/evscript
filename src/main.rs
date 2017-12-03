extern crate clap;
#[macro_use]
extern crate dyon;
#[macro_use]
extern crate evdev;
extern crate nix;
extern crate range;
extern crate rusty_sandbox;

use std::{env, io, fs, path, process};
use std::sync::{Arc, Mutex};
use evdev::{data, raw, uinput, Device};
use nix::unistd;
use nix::poll::{poll, EventFlags, PollFd, POLLIN};
use dyon::{error, load_str, Array, Dfn, FnIndex, Lt, Module, Object, Runtime, RustObject, Type, Variable};
use dyon::ast::Current;
use range::Range;

macro_rules! module_add {
    ($mod:ident << $fun:ident [$($lt:expr),*] [$($ty:expr),*] $ret:expr) => {
        $mod.add(
            Arc::new(stringify!($fun).into()),
            $fun,
            Dfn { lts: vec![$($lt),*], tys: vec![$($ty),*], ret: $ret }
        )
    }
}

macro_rules! wrap_var {
    (rustobj, $value:expr) => {
        Variable::RustObject(Arc::new(Mutex::new($value)) as RustObject)
    };
    (arr $typ:ident, $value:expr) => {
        Variable::Array(Arc::new($value.into_iter().map(|o| wrap_var!($typ, o)).collect::<Vec<_>>()) as Array)
    };
}

macro_rules! main_current_add {
    ($rt:ident $main:ident << $mutability:ident $name:ident = $var:expr) => {{
        let input_var : Arc<String> = Arc::new(stringify!($name).into());
        $main.currents.push(Current {
            name: input_var.clone(),
            source_range: Range::empty(0),
            mutable: $mutability,
        });
        $rt.local_stack.push((input_var.clone(), $rt.stack.len()));
        $rt.current_stack.push((input_var, $rt.stack.len()));
        $rt.stack.push($var);
    }};
    ($rt:ident $main:ident << $name:ident ($($typ:ident) +) = $obj:expr) => {
        main_current_add!($rt $main << false $name = wrap_var!($($typ) +, $obj))
    };
}

macro_rules! with_unwrapped_device {
    ($thing:expr, $fn:expr) => {
        match $thing {
            &Variable::RustObject(ref o) => {
                let mut guard = o.lock().expect(".lock()");
                let dev = guard.downcast_mut::<Device>().expect("downcast_mut()");
                ($fn)(dev)
            },
            ref x => panic!("What is this?? {:?}", x),
        }
    }
}

pub struct InputEvent {
    pub kind: u32,
    pub code: u32,
    pub value: u32,
}

dyon_obj!{InputEvent { kind, code, value }}

dyon_fn!{fn device_name(obj: RustObject) -> String {
    let mut guard = obj.lock().expect(".lock()");
    let dev = guard.downcast_mut::<Device>().expect(".downcast_mut()");
    let name = std::str::from_utf8(dev.name().to_bytes()).expect("from_utf8()").to_owned();
    name
}}

dyon_fn!{fn next_events(arr: Vec<Variable>) -> Vec<InputEvent> {
    loop {
        let mut pfds = arr.iter().map(|var| PollFd::new(
                with_unwrapped_device!(var, |dev : &mut Device| dev.fd()), POLLIN)).collect::<Vec<_>>();
        let _ = poll(&mut pfds, -1).expect("poll()");
        if let Some(i) = pfds.iter().position(|pfd| pfd.revents().unwrap_or(EventFlags::empty()).contains(POLLIN)) {
            let evts = with_unwrapped_device!(
                &arr[i as usize], |dev : &mut Device| dev.events().expect(".events()").collect::<Vec<_>>());
            return evts.iter().map(|evt| InputEvent {
                kind: evt._type as u32,
                code: evt.code as u32,
                value: evt.value as u32,
            }).collect::<Vec<_>>()
        }
    }
}}

dyon_fn!{fn emit_event(obj: RustObject, evt_v: Variable) -> bool {
    let mut guard = obj.lock().expect(".lock()");
    let dev = guard.downcast_mut::<uinput::Device>().expect(".downcast_mut()");
    if let Variable::Object(evt) = evt_v {
    match (evt.get(&Arc::new("kind".into())), evt.get(&Arc::new("code".into())), evt.get(&Arc::new("value".into()))) {
            (Some(&Variable::F64(kind, _)), Some(&Variable::F64(code, _)), Some(&Variable::F64(value, _))) => {
                let mut event = raw::input_event::default();
                event._type = kind as u16;
                event.code = code as u16;
                event.value = value as i32;
                dev.write_raw(event).expect("uinput write_raw()");
                true
            },
            x => {
                println!("WARNING: emit_event: event {:?} does not contain all of (kind, code, value) or one of them isn't a number {:?}", evt, x);
                false
            },
        }
    } else {
        println!("WARNING: emit_event: event is not an object");
        false
    }
}}

fn run_script(devs: Vec<Device>, uinput: uinput::Device, script_name: &str, script: String) {
    let mut module = Module::new();
    module_add!(module << device_name [Lt::Default] [Type::Any] Type::Text);
    module_add!(module << next_events [Lt::Default] [Type::Array(Box::new(Type::Any))] Type::Object);
    module_add!(module << emit_event [Lt::Default, Lt::Default] [Type::Any, Type::Object] Type::Bool);
    error(load_str("stdlib.dyon", Arc::new(include_str!("stdlib.dyon").into()), &mut module));
    error(load_str(script_name, Arc::new(script), &mut module));
    let mut rt = Runtime::new();
    match module.find_function(&Arc::new("main".into()), 0) {
        FnIndex::Loaded(i) => {
            let main_fun = &mut module.functions[i as usize];
            main_current_add!(rt main_fun << evdevs (arr rustobj) = devs);
            main_current_add!(rt main_fun << uinput (rustobj) = uinput);
        },
        x => panic!("Weird main function: {:?}", x),
    }
    error(rt.run(&Arc::new(module)));
}

fn drop_privileges() {
    if unistd::geteuid().is_root() {
        unistd::setgid(unistd::getgid()).expect("setegid()");
        unistd::setgroups(&[]).expect("setgroups()");
        unistd::chdir("/dev/input".into()).expect("chdir()");
        unistd::chroot("/dev/input".into()).expect("chroot()");
        unistd::setuid(unistd::getuid()).expect("setegid()");
    }
    rusty_sandbox::Sandbox::new().sandbox_this_process();
}

fn main() {
    let matches = clap::App::new("evscript")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Greg V <greg@unrelenting.technology>")
        .about("A tiny sandboxed Dyon scripting environment for evdev input devices.")
        .arg(
            clap::Arg::with_name("FILE")
                .short("f")
                .long("file")
                .takes_value(true)
                .help("The script file run, by default - (stdin)"),
        )
        .arg(
            clap::Arg::with_name("DEV")
                .short("d")
                .long("device")
                .takes_value(true)
                .multiple(true)
                .help("A device to get events from"),
        )
        .get_matches();

    if !matches.is_present("DEV") {
        eprintln!("No devices provided, exiting. Run with -h to see usage info.");
        process::exit(1);
    }

    let mut script_src : Box<io::Read> = match matches.value_of("FILE") {
        Some("-") | None => Box::new(io::stdin()),
        Some(x) => Box::new(fs::File::open(x).expect("open()")),
    };

    let devs = matches
        .values_of_os("DEV")
        .expect(".values_of_os()")
        .map(|a| evdev::Device::open(&a).expect("evdev open()"))
        .collect::<Vec<_>>();

    let uinput_path = env::var_os("EVSCRIPT_UINPUT_PATH")
        .and_then(|s| s.into_string().ok())
        .unwrap_or("/dev/uinput".to_owned());
    let ubuilder = uinput::Builder::new(&path::Path::new(&uinput_path)).expect("uinput Builder");

    drop_privileges();

    let mut script = String::new();
    let _ = script_src
        .read_to_string(&mut script)
        .expect("read_to_string");
    let mut conf = raw::uinput_setup::default();
    conf.set_name("Devicey McDeviceFace").expect("set_name");
    conf.id.bustype = 0x6;
    conf.id.vendor = 69;
    // TODO: read allowed events as a toml comment from the script instead of allowing all keys
    // also can read device name/vendor/product from there
    uinput_ioctl!(ui_set_evbit(ubuilder.fd(), data::KEY.number())).expect("ioctl");
    for i in 0..255 {
        uinput_ioctl!(ui_set_keybit(ubuilder.fd(), i)).expect("ioctl");
    }
    let uinput = ubuilder.setup(conf).expect("uinput setup()");
    run_script(devs, uinput, matches.value_of("FILE").unwrap_or("-"), script);
}
