use std::io;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread;
use std::process::exit;
use std::time::{SystemTime, Duration};

use websocket::{Client, Message, Sender};
use websocket::client::Sender as SenderObj;
use websocket::client::Receiver as ReceiverObj;
use websocket::client::request::{Request, Url};
use websocket::stream::WebSocketStream;
use openssl::ssl::{SslMethod, SslContext};

use ws;
use options::Options;
use frame_data::FrameData;
use http::{fetch_session_cookie, print_headers};

pub fn run_wsta(options: &mut Options) {

    // Get the URL
    log!(2, "About to unwrap: {}", options.url);
    let url = match Url::parse(&options.url) {
        Ok(res) => res,
        Err(err) => {
            log!(1, "Error object: {:?}", err);
            stderr!("An error occured while parsing '{}' as a WS URL: {}",
                    options.url, err);
            exit(1);
        }
    };
    log!(3, "Parsed URL: {:?}", url);

    let origin = get_origin(&url);
    log!(3, "Parsed Origin string: {}", origin);

    // Connect to the server
    log!(2, "About to connect to {}", url);
    let mut request;
    if !options.cipher_list.is_empty() || options.rsa_only {
        let mut ctx = SslContext::new(SslMethod::Sslv23).unwrap();
        if !options.cipher_list.is_empty() {
            log!(2, "Using ssl cipher_list {}", options.cipher_list);
            ctx.set_cipher_list(&options.cipher_list).unwrap();
        }
        else if options.rsa_only {
            log!(2, "Using RSA only cipher suites for ssl key exchange");
            ctx.set_cipher_list("AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA256:AES128-SHA:AES256-SHA:DES-CBC3-SHA").unwrap();
        }
        request = match Client::connect_ssl_context(url, &ctx) {
            Ok(res) => res,
            Err(err) => {
                log!(1, "Error: {:?}", err);
                stderr!("An error occured while connecting to '{}': {}",
                               options.url, err);
                exit(1);
            }
        };
    }
    else {
        request = match Client::connect(url) {
            Ok(res) => res,
            Err(err) => {
                log!(1, "Error: {:?}", err);
                stderr!("An error occured while connecting to '{}': {}",
                               options.url, err);
                exit(1);
            }
        };
    }

    // Set Origin header to be equal to the websocket url
    request.headers.set_raw("Origin", vec![origin.into_bytes()]);

    // Authenticate if requested
    if !options.login_url.is_empty() {
        let session_cookie = fetch_session_cookie(options);
        log!(2, "Got session cookie: {:?}", session_cookie);

        if session_cookie.is_some() {
            request.headers.set(session_cookie.unwrap());
            log!(3, "Session cookie set on request. Headers are now: {:?}",
                 request.headers);
        } else {
            log!(1, "session_cookie object: {:?}", session_cookie);

            stderr!(concat!("Attempted to fetch session cookie, but no ",
              "cookies were found in response's SetCookie header.",
              "Try looking at -I"));
            exit(1);
        }
    }

    // Add the headers passed from command line arguments
    if !options.headers.is_empty() {
        add_headers_to_request(&mut request, &mut options.headers);
    }

    // Print request
    if options.print_headers {
        print_headers("WebSocket upgrade request", &request.headers, None);
    }

    // Send the request
    log!(3, "About to send and unwrap request");
    let response = match request.send() {
        Ok(response) => {
            log!(3, "Request sent");

            response
        },
        Err(err) => {
            log!(1, "Error object: {:?}", err);
            stderr!("An error occured when connecting: {}", err);
            exit(1);
        }
    };

    // Dump headers when requested
    if options.print_headers {
        print_headers("WebSocket upgrade response",
                      &response.headers, Some(response.status));
    }

    // Ensure the response is valid and show an error if not
    match response.validate() {
        Err(error) => {
            log!(1, "Invalid reponse: {:?}", error);
            stderr!("{}", error);

            if !options.print_headers {
                stderr!("Try using -I for more info");
            }

            exit(1);
        },
        _ => stderr!("Connected to {}", options.url)
    }

    // Get a Client
    let client = response.begin();
    log!(3, "Client created");

    // Send message
    let (mut sender, receiver) = client.split();

    // Send pre-provided messages if preesnt
    if !options.messages.is_empty() {
        send_messages(&mut sender, &mut options.messages, options.echo);
    }

    ws::spawn_websocket_reader::<ReceiverObj<WebSocketStream>>(receiver);

    // Share mutable data between writer thread and main thread
    // using a lockable Mutex.
    // Mutex will block threads waiting for the lock to become available
    let stdin_buffer = ws::spawn_stdin_reader::<Arc<Mutex<Vec<FrameData>>>>
        (options.echo, options.binary_mode, options.binary_frame_size.clone());

    // Variables for checking against a ping interval
    let ping_interval = options.ping_interval.map(|i| Duration::from_secs(i));
    let mut last_time = SystemTime::now();

    log!(3, "Entering main loop");
    loop {

        // Read buffer, and send message to server if buffer contains anything
        ws::read_stdin_buffer(&mut sender, stdin_buffer.clone());

        // Check if ping_interval has passed, if so, send a ping frame
        last_time = ws::check_ping_interval(&ping_interval, last_time,
                                            &mut sender, options.echo,
                                            &options.ping_msg);

        // Sleep for 0.25 seconds at a time, to give the processor some rest.
        // Should be a multiple of 1 second as this is the smallest possible
        // ping_interval that can be input
        thread::sleep(Duration::from_millis(250));
    }
}

/// Parses an Origin string from a websocket URL, replacing ws[s] with http[s].
fn get_origin(url: &Url) -> String {
    let scheme = if url.scheme() == "wss" {
        "https"
    } else {
        "http"
    };

    format!("{}://{}", scheme, url.host_str().unwrap_or(""))
}

fn add_headers_to_request(request: &mut Request<WebSocketStream, WebSocketStream>,
                          headers: &mut Vec<String>) {

    log!(2, "Adding headers to request: {:?}", headers);
    for header in headers {

        // Only process the header if it is a valid "key: value" header
        if header.contains(':') {

            // Split by first colon into [key, value]
            let split = header.splitn(2, ':').collect::<Vec<&str>>();
            log!(3, "Split header: {:?}", split);

            let key = split[0];
            log!(3, "Key is: {}", key);

            let val = split[1].to_string().into_bytes();
            log!(3, "Val is: {:?} (bytes)", val);

            // Write raw (untyped) header
            request.headers.set_raw(format!("{}", key), vec![val]);
            log!(2, "Wrote new header. Headers are now: {:?}", request.headers);
        } else {
            stderr!("Invalid header: {}. Must contain a colon (:)", header);
        }
    }
}

fn send_messages(sender: &mut SenderObj<WebSocketStream>,
                 messages: &mut Vec<String>,
                 echo: bool) {

    for message in messages {
        if echo {
            println!("> {}", message);
        }

        let frame = Message::text(message.as_str());
        match sender.send_message(&frame) {
            Err(err) => {
                log!(1, "Error object: {:?}", err);
                stderr!("An error occured while sending message {:?}: {}",
                        message, err);
                exit(1);
            },
            _ => {}
        };
    }
}

