use std::ops::Deref;
use std::{cell::RefCell, collections::VecDeque, rc::Rc, task::Waker};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{CloseEvent, ErrorEvent, MessageEvent, WebSocket};

pub async fn connect(url: &str) -> crate::Result<WebSocketStream> {
    let ws = WebSocketStream::connect(url)?;

    WebSocketStream::connect_impl(ws).await
}

#[derive(Clone)]
pub struct Inner(WebSocket);

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Deref for Inner {
    type Target = WebSocket;
    fn deref(&self) -> &WebSocket {
        &self.0
    }
}

impl From<WebSocket> for Inner {
    fn from(ws: WebSocket) -> Self {
        Inner(ws)
    }
}

pub struct WebSocketStream {
    inner: Inner,
    queue: Rc<RefCell<VecDeque<crate::Result<crate::Message>>>>,
    waker: Rc<RefCell<Option<Waker>>>,
}

unsafe impl Send for WebSocketStream {}
unsafe impl Sync for WebSocketStream {}

impl WebSocketStream {
    fn connect(url: &str) -> crate::Result<Inner> {
        match web_sys::WebSocket::new(url) {
            Err(_err) => Err(crate::Error::Url(
                crate::error::UrlError::UnsupportedUrlScheme,
            )),
            Ok(ws) => Ok(Inner(ws)),
        }
    }
    async fn connect_impl(ws: Inner) -> crate::Result<Self> {
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let (open_sx, open_rx) = futures_channel::oneshot::channel();

        {
            let on_open_callback = {
                let mut open_sx = Some(open_sx);
                Closure::wrap(Box::new(move |_event| {
                    open_sx.take().map(|open_sx| open_sx.send(()));
                }) as Box<dyn FnMut(web_sys::Event)>)
            };
            ws.set_onopen(Some(on_open_callback.as_ref().unchecked_ref()));
            on_open_callback.forget();
        }

        let (err_sx, err_rx) = futures_channel::oneshot::channel();

        {
            let on_error_callback = {
                let mut err_sx = Some(err_sx);
                Closure::wrap(Box::new(move |_error_event| {
                    err_sx.take().map(|err_sx| err_sx.send(()));
                }) as Box<dyn FnMut(ErrorEvent)>)
            };
            ws.set_onerror(Some(on_error_callback.as_ref().unchecked_ref()));
            on_error_callback.forget();
        }

        let result = futures_util::future::select(open_rx, err_rx).await;
        ws.set_onopen(None);
        ws.set_onerror(None);
        let ws = match result {
            futures_util::future::Either::Left((_, _)) => Ok(ws),
            futures_util::future::Either::Right((_, _)) => Err(crate::Error::ConnectionClosed),
        }?;

        let waker = Rc::new(RefCell::new(Option::<Waker>::None));
        let queue = Rc::new(RefCell::new(VecDeque::new()));

        {
            let on_message_callback = {
                let waker = Rc::clone(&waker);
                let queue = Rc::clone(&queue);
                Closure::wrap(Box::new(move |event: MessageEvent| {
                    let payload = std::convert::TryFrom::try_from(event);
                    queue.borrow_mut().push_back(payload);
                    if let Some(waker) = waker.borrow_mut().take() {
                        waker.wake();
                    }
                }) as Box<dyn FnMut(MessageEvent)>)
            };
            ws.set_onmessage(Some(on_message_callback.as_ref().unchecked_ref()));
            on_message_callback.forget();
        }

        {
            let on_error_callback = {
                let waker = Rc::clone(&waker);
                let queue = Rc::clone(&queue);
                Closure::wrap(Box::new(move |_error_event| {
                    queue
                        .borrow_mut()
                        .push_back(Err(crate::Error::ConnectionClosed));
                    if let Some(waker) = waker.borrow_mut().take() {
                        waker.wake();
                    }
                }) as Box<dyn FnMut(ErrorEvent)>)
            };
            ws.set_onerror(Some(on_error_callback.as_ref().unchecked_ref()));
            on_error_callback.forget();
        }

        {
            let on_close_callback = {
                let waker = Rc::clone(&waker);
                let queue = Rc::clone(&queue);
                Closure::wrap(Box::new(move |event: CloseEvent| {
                    queue.borrow_mut().push_back(Ok(crate::Message::Close(Some(
                        crate::message::CloseFrame {
                            code: event.code().into(),
                            reason: event.reason().into(),
                        },
                    ))));
                    if let Some(waker) = waker.borrow_mut().take() {
                        waker.wake();
                    }
                }) as Box<dyn FnMut(CloseEvent)>)
            };
            ws.set_onclose(Some(on_close_callback.as_ref().unchecked_ref()));
            on_close_callback.forget();
        }

        Ok(Self {
            inner: ws,
            queue,
            waker,
        })
    }
}

impl Drop for WebSocketStream {
    fn drop(&mut self) {
        let _r = self.inner.close();
        self.inner.set_onmessage(None);
        self.inner.set_onclose(None);
        self.inner.set_onerror(None);
    }
}

enum ReadyState {
    Closed,
    Closing,
    Connecting,
    Open,
}

impl std::convert::TryFrom<u16> for ReadyState {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            web_sys::WebSocket::CLOSED => Ok(Self::Closed),
            web_sys::WebSocket::CLOSING => Ok(Self::Closing),
            web_sys::WebSocket::OPEN => Ok(Self::Open),
            web_sys::WebSocket::CONNECTING => Ok(Self::Connecting),
            _ => Err(()),
        }
    }
}

mod stream {
    use super::ReadyState;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    impl futures_util::Stream for super::WebSocketStream {
        type Item = crate::Result<crate::Message>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if self.queue.borrow().is_empty() {
                *self.waker.borrow_mut() = Some(cx.waker().clone());

                match std::convert::TryFrom::try_from(self.inner.ready_state()) {
                    Ok(ReadyState::Open) => Poll::Pending,
                    _ => None.into(),
                }
            } else {
                self.queue.borrow_mut().pop_front().into()
            }
        }
    }

    impl futures_util::Sink<crate::Message> for super::WebSocketStream {
        type Error = crate::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            match std::convert::TryFrom::try_from(self.inner.ready_state()) {
                Ok(ReadyState::Open) => Ok(()).into(),
                _ => Err(crate::Error::ConnectionClosed).into(),
            }
        }

        fn start_send(self: Pin<&mut Self>, item: crate::Message) -> Result<(), Self::Error> {
            match std::convert::TryFrom::try_from(self.inner.ready_state()) {
                Ok(ReadyState::Open) => {
                    match item {
                        crate::Message::Text(text) => self
                            .inner
                            .send_with_str(&text)
                            .map_err(|_| crate::Error::Utf8)?,
                        crate::Message::Binary(bin) => self
                            .inner
                            .send_with_u8_array(&bin)
                            .map_err(|_| crate::Error::Utf8)?,
                        crate::Message::Close(frame) => match frame {
                            None => self
                                .inner
                                .close()
                                .map_err(|_| crate::Error::AlreadyClosed)?,
                            Some(frame) => self
                                .inner
                                .close_with_code_and_reason(frame.code.into(), &frame.reason)
                                .map_err(|_| crate::Error::AlreadyClosed)?,
                        },
                    }
                    Ok(())
                }
                _ => Err(crate::Error::ConnectionClosed),
            }
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Ok(()).into()
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.inner
                .close()
                .map_err(|_| crate::Error::AlreadyClosed)?;
            Ok(()).into()
        }
    }
}

impl std::convert::TryFrom<web_sys::MessageEvent> for crate::Message {
    type Error = crate::Error;

    fn try_from(event: MessageEvent) -> Result<Self, Self::Error> {
        match event.data() {
            payload if payload.is_instance_of::<js_sys::ArrayBuffer>() => {
                let buffer = js_sys::Uint8Array::new(payload.unchecked_ref());
                let mut v = vec![0; buffer.length() as usize];
                buffer.copy_to(&mut v);
                Ok(crate::Message::Binary(v))
            }
            payload if payload.is_string() => match payload.as_string() {
                Some(text) => Ok(crate::Message::Text(text)),
                None => Err(crate::Error::Utf8),
            },
            payload if payload.is_instance_of::<web_sys::Blob>() => {
                Err(crate::Error::BlobFormatUnsupported)
            }
            _ => Err(crate::Error::UnknownFormat),
        }
    }
}
