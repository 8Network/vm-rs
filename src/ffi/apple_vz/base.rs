//! base module

use std::marker::PhantomData;
use std::slice;
use std::str;

use block::Block;
use libc::c_void;
use objc::rc::StrongPtr;
use objc::runtime::{Object, BOOL, NO, YES};
use objc::{class, msg_send, sel, sel_impl};

#[link(name = "Virtualization", kind = "framework")]
extern "C" {}

#[link(name = "Foundation", kind = "framework")]
extern "C" {
    pub fn dispatch_queue_create(label: *const libc::c_char, attr: Id) -> Id;
    pub fn dispatch_sync(queue: Id, block: *mut c_void);
    pub fn dispatch_async(queue: Id, block: &Block<(), ()>);
}

pub type Id = *mut Object;
pub const NIL: Id = 0 as Id;

pub struct NSArray<T> {
    pub _phantom: PhantomData<T>,
    pub p: StrongPtr,
}

impl<T> NSArray<T> {
    pub fn array_with_objects(objects: Vec<Id>) -> NSArray<T> {
        unsafe {
            // arrayWithObjects:count: is a factory method returning autoreleased (+0).
            // Must use StrongPtr::retain to add +1 before taking ownership.
            let p = StrongPtr::retain(
                msg_send![class!(NSArray), arrayWithObjects:objects.as_slice().as_ptr() count:objects.len()],
            );
            NSArray {
                p,
                _phantom: PhantomData,
            }
        }
    }

    pub fn count(&self) -> usize {
        unsafe { msg_send![*self.p, count] }
    }
}

impl<T: From<StrongPtr>> NSArray<T> {
    pub fn object_at_index(&self, index: usize) -> T {
        debug_assert!(index < self.count());
        unsafe { T::from(StrongPtr::retain(msg_send![*self.p, objectAtIndex: index])) }
    }
}

const UTF8_ENCODING: usize = 4;
pub struct NSString(pub StrongPtr);

impl NSString {
    pub fn new(string: &str) -> NSString {
        unsafe {
            let alloc: Id = msg_send![class!(NSString), alloc];
            let p = StrongPtr::new(
                msg_send![alloc, initWithBytes:string.as_ptr() length:string.len() encoding:UTF8_ENCODING as Id],
            );
            NSString(p)
        }
    }

    pub fn len(&self) -> usize {
        unsafe { msg_send![*self.0, lengthOfBytesUsingEncoding: UTF8_ENCODING] }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_str(&self) -> &str {
        unsafe {
            let bytes = {
                let bytes: *const libc::c_char = msg_send![*self.0, UTF8String];
                bytes as *const u8
            };
            let len = self.len();
            let bytes = slice::from_raw_parts(bytes, len);
            str::from_utf8(bytes).expect("NSString contained invalid UTF-8")
        }
    }
}

impl From<StrongPtr> for NSString {
    fn from(p: StrongPtr) -> Self {
        NSString(p)
    }
}

pub struct NSURL(pub StrongPtr);

impl NSURL {
    pub fn url_with_string(url: &str) -> NSURL {
        unsafe {
            let url_nsstring = NSString::new(url);
            let p = StrongPtr::retain(msg_send![class!(NSURL), URLWithString: url_nsstring]);
            NSURL(p)
        }
    }

    pub fn file_url_with_path(path: &str, is_directory: bool) -> NSURL {
        unsafe {
            let path_nsstring = NSString::new(path);
            let is_directory_ = if is_directory { YES } else { NO };
            let p = StrongPtr::retain(
                msg_send![class!(NSURL), fileURLWithPath:path_nsstring isDirectory:is_directory_],
            );
            NSURL(p)
        }
    }

    pub fn check_resource_is_reachable_and_return_error(&self) -> bool {
        unsafe {
            let b: BOOL = msg_send![*self.0, checkResourceIsReachableAndReturnError: NIL];
            b == YES
        }
    }

    pub fn absolute_url(&self) -> NSURL {
        unsafe {
            let p = StrongPtr::retain(msg_send![*self.0, absoluteURL]);
            NSURL(p)
        }
    }
}

pub struct NSFileHandle(pub StrongPtr);

impl Default for NSFileHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl NSFileHandle {
    pub fn new() -> NSFileHandle {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(NSFileHandle), new]);
            NSFileHandle(p)
        }
    }

    pub fn file_handle_with_standard_input() -> NSFileHandle {
        unsafe {
            let p = StrongPtr::retain(msg_send![class!(NSFileHandle), fileHandleWithStandardInput]);
            NSFileHandle(p)
        }
    }

    pub fn file_handle_with_standard_output() -> NSFileHandle {
        unsafe {
            let p = StrongPtr::retain(msg_send![
                class!(NSFileHandle),
                fileHandleWithStandardOutput
            ]);
            NSFileHandle(p)
        }
    }

    /// Create an `NSFileHandle` that does NOT take ownership of the fd.
    /// The caller must ensure the fd outlives this handle.
    ///
    /// # Safety
    /// `fd` must be a valid, open file descriptor.
    pub unsafe fn file_handle_with_fd_borrowed(fd: i32) -> NSFileHandle {
        let alloc: Id = msg_send![class!(NSFileHandle), alloc];
        let p = StrongPtr::new(msg_send![alloc, initWithFileDescriptor: fd closeOnDealloc: NO]);
        NSFileHandle(p)
    }

    /// Create an `NSFileHandle` that takes ownership of the fd.
    /// The fd will be closed when the Objective-C object is deallocated.
    ///
    /// # Safety
    /// `fd` must be a valid, open file descriptor. The caller must NOT close it
    /// after this call — ownership transfers to the `NSFileHandle`.
    pub unsafe fn file_handle_with_fd_owned(fd: i32) -> NSFileHandle {
        let alloc: Id = msg_send![class!(NSFileHandle), alloc];
        let p = StrongPtr::new(msg_send![alloc, initWithFileDescriptor: fd closeOnDealloc: YES]);
        NSFileHandle(p)
    }
}

pub struct NSDictionary(pub StrongPtr);

impl NSDictionary {
    /// Create an NSDictionary from key-value pairs.
    ///
    /// # Safety
    /// All `Id` values in `pairs` must be valid, retained Objective-C objects.
    pub fn from_pairs(pairs: &[(Id, Id)]) -> NSDictionary {
        unsafe {
            let keys: Vec<Id> = pairs.iter().map(|(k, _)| *k).collect();
            let values: Vec<Id> = pairs.iter().map(|(_, v)| *v).collect();
            // dictionaryWithObjects:forKeys:count: is a factory method returning autoreleased (+0).
            let p = StrongPtr::retain(msg_send![
                class!(NSDictionary),
                dictionaryWithObjects:values.as_ptr()
                forKeys:keys.as_ptr()
                count:pairs.len()
            ]);
            NSDictionary(p)
        }
    }

    pub fn all_keys<T>(&self) -> NSArray<T> {
        unsafe {
            NSArray {
                p: StrongPtr::retain(msg_send![*self.0, allKeys]),
                _phantom: PhantomData,
            }
        }
    }

    pub fn all_values<T>(&self) -> NSArray<T> {
        unsafe {
            NSArray {
                p: StrongPtr::retain(msg_send![*self.0, allValues]),
                _phantom: PhantomData,
            }
        }
    }
}

pub struct NSError(pub StrongPtr);

impl NSError {
    pub fn nil() -> NSError {
        unsafe {
            let p = StrongPtr::new(NIL);
            NSError(p)
        }
    }

    pub fn code(&self) -> isize {
        if *self.0 == NIL {
            return 0;
        }
        unsafe { msg_send![*self.0, code] }
    }

    pub fn localized_description(&self) -> NSString {
        unsafe { NSString(StrongPtr::retain(msg_send![*self.0, localizedDescription])) }
    }

    pub fn localized_failure_reason(&self) -> NSString {
        unsafe {
            NSString(StrongPtr::retain(msg_send![
                *self.0,
                localizedFailureReason
            ]))
        }
    }

    pub fn localized_recovery_suggestion(&self) -> NSString {
        unsafe {
            NSString(StrongPtr::retain(msg_send![
                *self.0,
                localizedRecoverySuggestion
            ]))
        }
    }

    pub fn help_anchor(&self) -> NSString {
        unsafe { NSString(StrongPtr::retain(msg_send![*self.0, helpAnchor])) }
    }

    pub fn user_info(&self) -> NSDictionary {
        unsafe { NSDictionary(StrongPtr::retain(msg_send![*self.0, userInfo])) }
    }

    pub fn dump(&self) {
        if *self.0 == NIL {
            println!("NSError: nil");
            return;
        }
        let code = self.code();
        println!("code: {}", code);

        // Helper: safely print an NSString that may be backed by nil
        fn safe_str(s: &NSString) -> &str {
            if *s.0 == NIL {
                "(nil)"
            } else {
                s.as_str()
            }
        }

        let desc = self.localized_description();
        println!("localizedDescription : {}", safe_str(&desc));
        let reason = self.localized_failure_reason();
        println!("localizedFailureReason : {}", safe_str(&reason));
        let suggestion = self.localized_recovery_suggestion();
        println!("localizedRecoverySuggestion : {}", safe_str(&suggestion));
        let anchor = self.help_anchor();
        println!("helpAnchor : {}", safe_str(&anchor));
    }

    /// Create a synthetic NSError with a domain and description.
    ///
    /// Used when an ObjC API returns nil without an error object (macOS 16+).
    pub fn from_description(domain: &str, description: &str) -> Self {
        unsafe {
            let ns_domain = NSString::new(domain);
            let ns_desc = NSString::new(description);
            let desc_key = NSString::new("NSLocalizedDescription");
            let user_info = NSDictionary::from_pairs(&[(*desc_key.0, *ns_desc.0)]);
            let p: Id = msg_send![class!(NSError), errorWithDomain:*ns_domain.0
                                                   code:(-1isize)
                                                   userInfo:*user_info.0];
            NSError(StrongPtr::retain(p))
        }
    }
}
