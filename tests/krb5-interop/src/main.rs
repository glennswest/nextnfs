//! Drives nextnfs's RPCSEC_GSS with MIT Kerberos's own GSS-API
//! (`libgssapi_krb5.so.2`, loaded at run time), the library rpc.gssd
//! uses to set up a Linux NFS client's contexts.
//!
//! usage: krb5-interop <host:port> <service principal> <export>
//!
//! With a ticket cache holding a TGT (KRB5CCNAME), for each service
//! (krb5, krb5i, krb5p) it establishes a context (RPCSEC_GSS_INIT, mutual
//! authentication), checks the INIT verifier, sends COMPOUNDs (PUTROOTFH,
//! LOOKUP <export>, GETFH; then READDIR of the export) and checks every
//! reply's verifier and its integrity checksum or decryption, then
//! RPCSEC_GSS_DESTROY. Also checks that AUTH_SYS gets NFS4ERR_WRONGSEC
//! on the export. Exits non-zero on the first failure.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::ptr;

type OmUint32 = u32;

#[repr(C)]
struct GssBuffer {
    length: usize,
    value: *mut c_void,
}

#[repr(C)]
struct GssOid {
    length: u32,
    elements: *mut c_void,
}

type ImportName = unsafe extern "C" fn(*mut OmUint32, *mut GssBuffer, *const GssOid, *mut *mut c_void) -> OmUint32;
type InitSecContext = unsafe extern "C" fn(
    *mut OmUint32,
    *mut c_void,
    *mut *mut c_void,
    *mut c_void,
    *const GssOid,
    OmUint32,
    OmUint32,
    *mut c_void,
    *mut GssBuffer,
    *mut *const GssOid,
    *mut GssBuffer,
    *mut OmUint32,
    *mut OmUint32,
) -> OmUint32;
type GetMic = unsafe extern "C" fn(*mut OmUint32, *mut c_void, OmUint32, *mut GssBuffer, *mut GssBuffer) -> OmUint32;
type VerifyMic = unsafe extern "C" fn(*mut OmUint32, *mut c_void, *mut GssBuffer, *mut GssBuffer, *mut OmUint32) -> OmUint32;
type Wrap = unsafe extern "C" fn(*mut OmUint32, *mut c_void, i32, OmUint32, *mut GssBuffer, *mut i32, *mut GssBuffer) -> OmUint32;
type Unwrap = unsafe extern "C" fn(*mut OmUint32, *mut c_void, *mut GssBuffer, *mut GssBuffer, *mut i32, *mut OmUint32) -> OmUint32;
type ReleaseBuffer = unsafe extern "C" fn(*mut OmUint32, *mut GssBuffer) -> OmUint32;
type DisplayStatus =
    unsafe extern "C" fn(*mut OmUint32, OmUint32, i32, *const GssOid, *mut OmUint32, *mut GssBuffer) -> OmUint32;

const GSS_C_MUTUAL_FLAG: u32 = 2;
const GSS_C_REPLAY_FLAG: u32 = 4;
const GSS_C_SEQUENCE_FLAG: u32 = 8;
const GSS_C_CONF_FLAG: u32 = 16;
const GSS_C_INTEG_FLAG: u32 = 32;

struct Gss {
    _lib: libloading::Library,
    import_name: ImportName,
    init: InitSecContext,
    get_mic: GetMic,
    verify_mic: VerifyMic,
    wrap: Wrap,
    unwrap: Unwrap,
    release: ReleaseBuffer,
    display: DisplayStatus,
    nt_principal: *const GssOid,
}

fn buf(data: &[u8]) -> GssBuffer {
    GssBuffer { length: data.len(), value: data.as_ptr() as *mut c_void }
}

fn empty() -> GssBuffer {
    GssBuffer { length: 0, value: ptr::null_mut() }
}

impl Gss {
    fn load() -> Gss {
        unsafe {
            let lib = libloading::Library::new("libgssapi_krb5.so.2").expect("load libgssapi_krb5.so.2");
            macro_rules! f {
                ($name:literal) => {
                    *lib.get($name).unwrap_or_else(|e| fail(&format!("symbol: {}", e)))
                };
            }
            let nt_principal: *const GssOid = {
                let nt: libloading::Symbol<*const *const GssOid> =
                    lib.get(b"GSS_KRB5_NT_PRINCIPAL_NAME").unwrap_or_else(|e| fail(&format!("symbol: {}", e)));
                **nt
            };
            Gss {
                import_name: f!(b"gss_import_name"),
                init: f!(b"gss_init_sec_context"),
                get_mic: f!(b"gss_get_mic"),
                verify_mic: f!(b"gss_verify_mic"),
                wrap: f!(b"gss_wrap"),
                unwrap: f!(b"gss_unwrap"),
                release: f!(b"gss_release_buffer"),
                display: f!(b"gss_display_status"),
                nt_principal,
                _lib: lib,
            }
        }
    }

    fn take(&self, b: &mut GssBuffer) -> Vec<u8> {
        let v = if b.length == 0 { Vec::new() } else { unsafe { std::slice::from_raw_parts(b.value as *const u8, b.length).to_vec() } };
        let mut minor = 0;
        unsafe { (self.release)(&mut minor, b) };
        v
    }

    fn status(&self, major: u32, minor: u32) -> String {
        let mut out = Vec::new();
        for (code, kind) in [(major, 1), (minor, 2)] {
            let mut ctx = 0;
            let mut m = 0;
            let mut b = empty();
            unsafe { (self.display)(&mut m, code, kind, ptr::null(), &mut ctx, &mut b) };
            out.push(String::from_utf8_lossy(&self.take(&mut b)).into_owned());
        }
        format!("major {:#x} minor {}: {}", major, minor, out.join("; "))
    }

    fn check(&self, what: &str, major: u32, minor: u32) {
        if major != 0 {
            fail(&format!("{} failed: {}", what, self.status(major, minor)));
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("FAIL: {}", msg);
    std::process::exit(1);
}

// ------------------------------------------------------------------ RPC

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}

fn put_opaque(b: &mut Vec<u8>, d: &[u8]) {
    put_u32(b, d.len() as u32);
    b.extend_from_slice(d);
    b.resize(b.len() + (4 - d.len() % 4) % 4, 0);
}

fn get_u32(b: &[u8], pos: &mut usize) -> u32 {
    let v = u32::from_be_bytes(b.get(*pos..*pos + 4).unwrap_or_else(|| fail("short reply")).try_into().unwrap());
    *pos += 4;
    v
}

fn get_opaque(b: &[u8], pos: &mut usize) -> Vec<u8> {
    let n = get_u32(b, pos) as usize;
    let v = b.get(*pos..*pos + n).unwrap_or_else(|| fail("short opaque")).to_vec();
    *pos += n + (4 - n % 4) % 4;
    v
}

fn rpc(stream: &mut TcpStream, msg: &[u8]) -> Vec<u8> {
    let mut rec = ((msg.len() as u32) | 0x8000_0000).to_be_bytes().to_vec();
    rec.extend_from_slice(msg);
    stream.write_all(&rec).unwrap_or_else(|e| fail(&format!("send: {}", e)));
    let mut out = Vec::new();
    loop {
        let mut h = [0u8; 4];
        stream.read_exact(&mut h).unwrap_or_else(|e| fail(&format!("receive: {}", e)));
        let h = u32::from_be_bytes(h);
        let mut frag = vec![0u8; (h & 0x7fff_ffff) as usize];
        stream.read_exact(&mut frag).unwrap_or_else(|e| fail(&format!("receive: {}", e)));
        out.extend_from_slice(&frag);
        if h & 0x8000_0000 != 0 {
            return out;
        }
    }
}

struct Reply {
    verf_flavor: u32,
    verf: Vec<u8>,
    results: Vec<u8>,
}

/// Parses an RPC reply; fails unless it is MSG_ACCEPTED / SUCCESS.
fn parse_reply(r: &[u8], xid: u32) -> Reply {
    let mut p = 0;
    if get_u32(r, &mut p) != xid || get_u32(r, &mut p) != 1 {
        fail("not a reply to this call");
    }
    if get_u32(r, &mut p) != 0 {
        let what = get_u32(r, &mut p);
        let stat = get_u32(r, &mut p);
        fail(&format!("MSG_DENIED ({} {})", what, stat));
    }
    let verf_flavor = get_u32(r, &mut p);
    let verf = get_opaque(r, &mut p);
    let stat = get_u32(r, &mut p);
    if stat != 0 {
        fail(&format!("accept_stat {}", stat));
    }
    Reply { verf_flavor, verf, results: r[p..].to_vec() }
}

fn call_header(xid: u32, proc: u32, flavor: u32, cred: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    for w in [xid, 0, 2, 100003, 4, proc, flavor] {
        put_u32(&mut b, w);
    }
    put_opaque(&mut b, cred);
    b
}

fn gss_cred(proc: u32, seq: u32, service: u32, handle: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    for w in [1, proc, seq, service] {
        put_u32(&mut b, w);
    }
    put_opaque(&mut b, handle);
    b
}

/// COMPOUND (minor version 0): PUTROOTFH, LOOKUP export, then `tail` ops.
fn compound(export: &str, readdir: bool) -> Vec<u8> {
    let mut b = Vec::new();
    put_opaque(&mut b, b"interop");
    put_u32(&mut b, 0);
    put_u32(&mut b, 3);
    put_u32(&mut b, 24); // PUTROOTFH
    put_u32(&mut b, 15); // LOOKUP
    put_opaque(&mut b, export.as_bytes());
    if readdir {
        put_u32(&mut b, 26); // READDIR cookie, cookieverf, dircount, maxcount, attr bitmap
        b.extend_from_slice(&[0u8; 16]);
        put_u32(&mut b, 8192);
        put_u32(&mut b, 65536);
        put_u32(&mut b, 1);
        put_u32(&mut b, 0x0000_0001 << 1); // TYPE (attr 1)
    } else {
        put_u32(&mut b, 10); // GETFH
    }
    b
}

/// COMPOUND status and tag check.
fn compound_status(results: &[u8]) -> u32 {
    let mut p = 0;
    let status = get_u32(results, &mut p);
    if get_opaque(results, &mut p) != b"interop" {
        fail("COMPOUND tag not echoed");
    }
    status
}

struct Context {
    ctx: *mut c_void,
    handle: Vec<u8>,
    seq: u32,
}

fn establish(gss: &Gss, stream: &mut TcpStream, target: &str, xid: &mut u32) -> Context {
    let mut minor = 0;
    let mut name = ptr::null_mut();
    let mut nb = buf(target.as_bytes());
    gss.check("gss_import_name", unsafe { (gss.import_name)(&mut minor, &mut nb, gss.nt_principal, &mut name) }, minor);
    let mut ctx: *mut c_void = ptr::null_mut();
    let flags = GSS_C_MUTUAL_FLAG | GSS_C_REPLAY_FLAG | GSS_C_SEQUENCE_FLAG | GSS_C_CONF_FLAG | GSS_C_INTEG_FLAG;
    let mut out = empty();
    let mut ret_flags = 0;
    let major = unsafe {
        (gss.init)(&mut minor, ptr::null_mut(), &mut ctx, name, ptr::null(), flags, 0, ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), &mut out, &mut ret_flags, ptr::null_mut())
    };
    if major != 1 {
        fail(&format!("gss_init_sec_context did not ask to continue: {}", gss.status(major, minor)));
    }
    let token = gss.take(&mut out);

    *xid += 1;
    let mut msg = call_header(*xid, 0, 6, &gss_cred(1, 0, 1, &[]));
    put_u32(&mut msg, 0);
    put_u32(&mut msg, 0);
    put_opaque(&mut msg, &token);
    let reply = parse_reply(&rpc(stream, &msg), *xid);
    let mut p = 0;
    let handle = get_opaque(&reply.results, &mut p);
    let (gmaj, gmin, window) = (get_u32(&reply.results, &mut p), get_u32(&reply.results, &mut p), get_u32(&reply.results, &mut p));
    let out_token = get_opaque(&reply.results, &mut p);
    if gmaj != 0 {
        fail(&format!("RPCSEC_GSS_INIT refused: gss_major {:#x} minor {}", gmaj, gmin));
    }
    let mut inb = buf(&out_token);
    let mut out2 = empty();
    let major = unsafe {
        (gss.init)(&mut minor, ptr::null_mut(), &mut ctx, name, ptr::null(), flags, 0, ptr::null_mut(), &mut inb, ptr::null_mut(), &mut out2, &mut ret_flags, ptr::null_mut())
    };
    gss.check("gss_init_sec_context (AP-REP)", major, minor);
    gss.take(&mut out2);
    if ret_flags & GSS_C_MUTUAL_FLAG == 0 {
        fail("mutual authentication not done");
    }
    // The INIT reply's verifier is the MIC of the window.
    if reply.verf_flavor != 6 {
        fail("INIT reply verifier is not RPCSEC_GSS");
    }
    let wb = window.to_be_bytes();
    let (mut m, mut t) = (buf(&wb), buf(&reply.verf));
    let mut qop = 0;
    gss.check("verify INIT verifier", unsafe { (gss.verify_mic)(&mut minor, ctx, &mut m, &mut t, &mut qop) }, minor);
    println!("  context established with {} (window {})", target, window);
    Context { ctx, handle, seq: 1 }
}

/// Sends one DATA call (`proc`, `args`) under `service`; returns the
/// unwrapped results.
fn data_call(gss: &Gss, stream: &mut TcpStream, c: &mut Context, service: u32, proc: u32, args: &[u8], xid: &mut u32) -> Vec<u8> {
    let mut minor = 0;
    *xid += 1;
    c.seq += 1;
    let seq = c.seq;
    let gss_proc = if proc == 0 && args.is_empty() && service == 0 { 3 } else { 0 };
    let service = if service == 0 { 1 } else { service };
    let mut msg = call_header(*xid, proc, 6, &gss_cred(gss_proc, seq, service, &c.handle));
    let mut hb = buf(&msg);
    let mut mic = empty();
    gss.check("gss_get_mic (header)", unsafe { (gss.get_mic)(&mut minor, c.ctx, 0, &mut hb, &mut mic) }, minor);
    let mic = gss.take(&mut mic);
    put_u32(&mut msg, 6);
    put_opaque(&mut msg, &mic);
    let mut body = seq.to_be_bytes().to_vec();
    body.extend_from_slice(args);
    match service {
        1 => msg.extend_from_slice(args),
        2 => {
            let mut bb = buf(&body);
            let mut cks = empty();
            gss.check("gss_get_mic (args)", unsafe { (gss.get_mic)(&mut minor, c.ctx, 0, &mut bb, &mut cks) }, minor);
            put_opaque(&mut msg, &body);
            put_opaque(&mut msg, &gss.take(&mut cks));
        }
        _ => {
            let mut bb = buf(&body);
            let mut tok = empty();
            let mut conf = 0;
            gss.check("gss_wrap (args)", unsafe { (gss.wrap)(&mut minor, c.ctx, 1, 0, &mut bb, &mut conf, &mut tok) }, minor);
            put_opaque(&mut msg, &gss.take(&mut tok));
        }
    }
    let reply = parse_reply(&rpc(stream, &msg), *xid);
    // Reply verifier: MIC of the sequence number.
    let sb = seq.to_be_bytes();
    let (mut m, mut t) = (buf(&sb), buf(&reply.verf));
    let mut qop = 0;
    if reply.verf_flavor != 6 {
        fail("reply verifier is not RPCSEC_GSS");
    }
    gss.check("verify reply verifier", unsafe { (gss.verify_mic)(&mut minor, c.ctx, &mut m, &mut t, &mut qop) }, minor);
    if gss_proc == 3 {
        return Vec::new();
    }
    let body = match service {
        1 => return reply.results,
        2 => {
            let mut p = 0;
            let body = get_opaque(&reply.results, &mut p);
            let cks = get_opaque(&reply.results, &mut p);
            let (mut bb, mut cb) = (buf(&body), buf(&cks));
            gss.check("verify reply checksum", unsafe { (gss.verify_mic)(&mut minor, c.ctx, &mut bb, &mut cb, &mut qop) }, minor);
            body
        }
        _ => {
            let mut p = 0;
            let tok = get_opaque(&reply.results, &mut p);
            let mut tb = buf(&tok);
            let mut out = empty();
            let mut conf = 0;
            gss.check("gss_unwrap (reply)", unsafe { (gss.unwrap)(&mut minor, c.ctx, &mut tb, &mut out, &mut conf, &mut qop) }, minor);
            if conf != 1 {
                fail("krb5p reply was not encrypted");
            }
            gss.take(&mut out)
        }
    };
    if body.get(..4) != Some(&sb[..]) {
        fail("sequence number in the reply body does not match");
    }
    body[4..].to_vec()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: krb5-interop <host:port> <service principal> <export>");
        std::process::exit(2);
    }
    let (addr, target, export) = (&args[1], &args[2], &args[3]);
    let gss = Gss::load();
    let mut stream = TcpStream::connect(addr).unwrap_or_else(|e| fail(&format!("connect {}: {}", addr, e)));
    let mut xid = 0x4000;

    // AUTH_SYS on the Kerberos-only export: WRONGSEC.
    xid += 1;
    let mut sys = Vec::new();
    put_u32(&mut sys, 0);
    put_opaque(&mut sys, b"interop");
    for w in [0, 0, 0] {
        put_u32(&mut sys, w);
    }
    let mut msg = call_header(xid, 1, 1, &sys);
    put_u32(&mut msg, 0);
    put_u32(&mut msg, 0);
    msg.extend_from_slice(&compound(export, false));
    let status = compound_status(&parse_reply(&rpc(&mut stream, &msg), xid).results);
    if status != 10016 {
        fail(&format!("AUTH_SYS on the export: status {}, expected NFS4ERR_WRONGSEC", status));
    }
    println!("ok  AUTH_SYS gets NFS4ERR_WRONGSEC");

    for (service, name) in [(1u32, "krb5"), (2, "krb5i"), (3, "krb5p")] {
        let mut c = establish(&gss, &mut stream, target, &mut xid);
        // NULL under the service, then the COMPOUNDs.
        data_call(&gss, &mut stream, &mut c, service, 0, &[], &mut xid);
        let r = data_call(&gss, &mut stream, &mut c, service, 1, &compound(export, false), &mut xid);
        let status = compound_status(&r);
        if status != 0 {
            fail(&format!("{}: PUTROOTFH/LOOKUP/GETFH status {}", name, status));
        }
        let r = data_call(&gss, &mut stream, &mut c, service, 1, &compound(export, true), &mut xid);
        let status = compound_status(&r);
        if status != 0 {
            fail(&format!("{}: READDIR status {}", name, status));
        }
        println!("ok  {}: COMPOUNDs verified ({} byte READDIR reply)", name, r.len());
        // DESTROY (service 0 marks it in data_call).
        data_call(&gss, &mut stream, &mut c, 0, 0, &[], &mut xid);
    }
    println!("PASS");
}
