#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{NSDate, NSRect, NSRunLoop, NSString, NSURL};
    use objc2_web_kit::{WKWebView, WKWebViewConfiguration};
    use std::{cell::RefCell, rc::Rc, time::Instant};
    let mtm = MainThreadMarker::new().ok_or("run on main thread")?;
    let _app = NSApplication::sharedApplication(mtm);
    let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
    let view: Retained<WKWebView> = unsafe {
        WKWebView::initWithFrame_configuration(
            WKWebView::alloc(mtm),
            NSRect::default(),
            &configuration,
        )
    };
    let captured = Rc::new(RefCell::new(Vec::new()));
    let sink = captured.clone();
    let capabilities = webview_diagnostics::webkit::attach(
        mtm,
        &view,
        "smoke",
        Rc::new(move |record| sink.borrow_mut().push(record)),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let server = std::thread::spawn(move || -> std::io::Result<()> {
        use std::io::{Read, Write};
        let start = Instant::now();
        while start.elapsed().as_secs() < 10 {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request)?;
                    let body = r#"{"ok":true}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )?;
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "no fixture request",
        ))
    });
    let base = NSURL::URLWithString(&NSString::from_str(&format!("http://127.0.0.1:{port}/")))
        .ok_or("invalid fixture URL")?;
    let html = format!(
        "<html><script>console.log('native-smoke-ok');fetch('http://127.0.0.1:{port}/data').then(r=>r.json()).then(d=>console.log('network-smoke-ok',d.ok));</script></html>"
    );
    unsafe {
        view.loadHTMLString_baseURL(&NSString::from_str(&html), Some(&base));
    }
    let started = Instant::now();
    while started.elapsed().as_secs()<10 && !captured.borrow().iter().any(|r|matches!(&r.entry,webview_diagnostics::Entry::Console{text,..} if text.contains("network-smoke-ok"))) {
  NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
 }
    let passed=captured.borrow().iter().any(|r|matches!(&r.entry,webview_diagnostics::Entry::Console{text,..} if text.contains("network-smoke-ok")));
    let native_request=captured.borrow().iter().any(|r|matches!(&r.entry,webview_diagnostics::Entry::Network{url,status:Some(200),..} if url.ends_with("/data")));
    webview_diagnostics::webkit::unloaded(mtm, &view, "smoke");
    server.join().map_err(|_| "fixture thread panicked")??;
    if capabilities.native_network && !native_request {
        return Err("native request capture did not arrive".into());
    }
    if !passed {
        return Err("real WebKit console capture did not arrive".into());
    }
    println!(
        "Real WKWebView console and HTTP fixture passed; native network available: {}",
        capabilities.native_network
    );
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    println!("WKWebView capture requires macOS");
}
