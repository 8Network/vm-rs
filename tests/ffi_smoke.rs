//! FFI smoke tests — verify all new Apple VZ FFI bindings can be
//! instantiated without crashing.
//!
//! These tests exercise the ObjC FFI layer directly. They verify:
//! - Objects can be created via alloc/init or factory methods
//! - retain/release semantics are correct (no double-free crashes)
//! - Data can be read back from objects
//!
//! macOS only — skipped on other platforms.
//! Requires Apple Virtualization.framework (macOS 12+).

#[cfg(target_os = "macos")]
mod apple_vz {
    // ─── Platform Configuration ─────────────────────────────────────────

    #[test]
    fn generic_platform_configuration_creates() {
        use vm_rs::ffi::apple_vz::platform::VZGenericPlatformConfiguration;
        let _platform = VZGenericPlatformConfiguration::new();
    }

    #[test]
    fn generic_machine_identifier_creates() {
        use vm_rs::ffi::apple_vz::platform::VZGenericMachineIdentifier;
        let id = VZGenericMachineIdentifier::new();
        let data = id.data_representation();
        assert!(
            !data.is_empty(),
            "machine identifier data should not be empty"
        );
    }

    #[test]
    fn generic_machine_identifier_roundtrip() {
        use vm_rs::ffi::apple_vz::platform::VZGenericMachineIdentifier;

        // Create → serialize → deserialize → compare
        let original = VZGenericMachineIdentifier::new();
        let data = original.data_representation();
        assert!(!data.is_empty());

        let restored = VZGenericMachineIdentifier::from_data(&data);
        assert!(
            restored.is_some(),
            "should be able to restore from valid data"
        );

        let restored = restored.unwrap();
        let restored_data = restored.data_representation();
        assert_eq!(
            data, restored_data,
            "roundtripped machine identifier should produce identical bytes"
        );
    }

    #[test]
    fn generic_machine_identifier_invalid_data() {
        use vm_rs::ffi::apple_vz::platform::VZGenericMachineIdentifier;

        let result = VZGenericMachineIdentifier::from_data(&[0xFF, 0x00]);
        // Invalid data should return None (not crash)
        // Note: VZ framework may accept short data — the important thing is no crash.
        let _ = result;
    }

    #[test]
    fn platform_with_machine_identifier() {
        use vm_rs::ffi::apple_vz::platform::{
            VZGenericMachineIdentifier, VZGenericPlatformConfiguration,
        };

        let mut platform = VZGenericPlatformConfiguration::new();
        let id = VZGenericMachineIdentifier::new();
        platform.set_machine_identifier(&id);
        // Should not crash — that's the test
    }

    // ─── vsock (Socket Device) ──────────────────────────────────────────

    #[test]
    fn vsock_device_configuration_creates() {
        use vm_rs::ffi::apple_vz::socket_device::VZVirtioSocketDeviceConfiguration;
        let _config = VZVirtioSocketDeviceConfiguration::new();
    }

    #[test]
    fn vsock_listener_creates() {
        use vm_rs::ffi::apple_vz::socket_device::VZVirtioSocketListener;
        let _listener = VZVirtioSocketListener::new();
    }

    #[test]
    fn vsock_config_implements_trait() {
        use vm_rs::ffi::apple_vz::socket_device::{
            VZSocketDeviceConfiguration, VZVirtioSocketDeviceConfiguration,
        };
        let config = VZVirtioSocketDeviceConfiguration::new();
        let id = config.id();
        assert!(!id.is_null(), "vsock config Id should not be null");
    }

    // ─── UEFI Boot ─────────────────────────────────────────────────────

    #[test]
    fn efi_variable_store_create_and_open() {
        use vm_rs::ffi::apple_vz::boot_loader::{VZEFIBootLoader, VZEFIVariableStore};

        let tmp = tempfile::tempdir().expect("tempdir");
        let store_path = tmp.path().join("nvram.bin");
        let store_path_str = store_path.to_str().unwrap();

        // Create a new variable store
        let store = VZEFIVariableStore::create(store_path_str);
        assert!(store.is_ok(), "should create EFI variable store");
        let store = match store {
            Ok(s) => s,
            Err(_) => panic!("should create EFI variable store"),
        };

        // Create boot loader with the store
        let _loader = VZEFIBootLoader::new(&store);

        // Re-open the existing store
        let store2 = VZEFIVariableStore::open(store_path_str);
        assert!(store2.is_ok(), "should open existing EFI variable store");
    }

    #[test]
    fn efi_variable_store_open_nonexistent_fails() {
        use vm_rs::ffi::apple_vz::boot_loader::VZEFIVariableStore;

        let result = VZEFIVariableStore::open("/nonexistent/path/nvram.bin");
        assert!(result.is_err(), "opening nonexistent store should fail");
    }

    // ─── Shared Directories (Rosetta + Multiple) ────────────────────────

    #[test]
    fn rosetta_availability_does_not_crash() {
        use vm_rs::ffi::apple_vz::shared_directory::{
            VZLinuxRosettaAvailability, VZLinuxRosettaDirectoryShare,
        };

        let avail = VZLinuxRosettaDirectoryShare::availability();
        // On Intel: NotSupported, on AS: Installed or NotInstalled
        assert!(matches!(
            avail,
            VZLinuxRosettaAvailability::NotSupported
                | VZLinuxRosettaAvailability::NotInstalled
                | VZLinuxRosettaAvailability::Installed
        ));
    }

    #[test]
    fn multiple_directory_share_creates() {
        use vm_rs::ffi::apple_vz::shared_directory::{
            VZMultipleDirectoryShare, VZSharedDirectory, VZVirtioFileSystemDeviceConfiguration,
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let share_a = VZSharedDirectory::new(dir_a.to_str().unwrap(), true);
        let share_b = VZSharedDirectory::new(dir_b.to_str().unwrap(), false);
        let multi = VZMultipleDirectoryShare::new(&[("config", share_a), ("data", share_b)]);

        let mut device = VZVirtioFileSystemDeviceConfiguration::new("shares");
        device.set_share(multi);
        // No crash = success
    }

    #[test]
    fn single_directory_share_with_trait() {
        use vm_rs::ffi::apple_vz::shared_directory::{
            VZDirectoryShare, VZSharedDirectory, VZSingleDirectoryShare,
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = VZSharedDirectory::new(tmp.path().to_str().unwrap(), true);
        let share = VZSingleDirectoryShare::new(dir);
        let id = share.id();
        assert!(!id.is_null(), "single share Id should not be null");
    }

    // ─── NVMe Storage ──────────────────────────────────────────────────

    #[test]
    fn nvme_device_configuration_creates() {
        use vm_rs::ffi::apple_vz::storage_device::{
            VZDiskImageStorageDeviceAttachmentBuilder, VZNVMExpressControllerDeviceConfiguration,
            VZStorageDeviceConfiguration,
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let disk = tmp.path().join("test.img");
        // Create a minimal disk image (just needs to exist)
        std::fs::write(&disk, vec![0u8; 4096]).unwrap();

        let attachment = VZDiskImageStorageDeviceAttachmentBuilder::new()
            .path(disk.to_str().unwrap())
            .read_only(true)
            .build();
        let attachment = match attachment {
            Ok(a) => a,
            Err(_) => panic!("disk attachment should build"),
        };

        let nvme = VZNVMExpressControllerDeviceConfiguration::new(attachment);
        let id = nvme.id();
        assert!(!id.is_null(), "NVMe config Id should not be null");
    }

    // ─── File Serial Port ──────────────────────────────────────────────

    #[test]
    fn file_serial_port_attachment_creates() {
        use vm_rs::ffi::apple_vz::serial_port::{VZFileSerialPortAttachment, VZSerialPortAttachment};

        let tmp = tempfile::tempdir().expect("tempdir");
        let log_path = tmp.path().join("serial.log");
        // Create the file first
        std::fs::write(&log_path, b"").unwrap();

        let attachment = VZFileSerialPortAttachment::new(log_path.to_str().unwrap(), false)
            .expect("VZFileSerialPortAttachment::new should succeed for a valid path");
        let id = attachment.id();
        assert!(!id.is_null(), "file serial port Id should not be null");
    }

    #[test]
    fn file_serial_port_attachment_append_mode() {
        use vm_rs::ffi::apple_vz::serial_port::VZFileSerialPortAttachment;

        let tmp = tempfile::tempdir().expect("tempdir");
        let log_path = tmp.path().join("serial-append.log");
        std::fs::write(&log_path, b"existing content\n").unwrap();

        let _attachment = VZFileSerialPortAttachment::new(log_path.to_str().unwrap(), true)
            .expect("VZFileSerialPortAttachment::new should succeed in append mode");
        // No crash in append mode = success
    }

    // ─── NSDictionary construction ─────────────────────────────────────

    #[test]
    fn nsdictionary_from_pairs() {
        use vm_rs::ffi::apple_vz::base::{NSDictionary, NSString};

        let key1 = NSString::new("hello");
        let val1 = NSString::new("world");
        let key2 = NSString::new("foo");
        let val2 = NSString::new("bar");

        let dict = NSDictionary::from_pairs(&[(*key1.0, *val1.0), (*key2.0, *val2.0)]);

        let keys = dict.all_keys::<NSString>();
        assert_eq!(keys.count(), 2, "dictionary should have 2 keys");
    }

    // ─── VM Configuration Builder with new features ────────────────────

    #[test]
    fn vm_config_builder_with_platform_and_vsock() {
        use vm_rs::ffi::apple_vz::{
            entropy_device::VZVirtioEntropyDeviceConfiguration,
            memory_device::VZVirtioTraditionalMemoryBalloonDeviceConfiguration,
            platform::{VZGenericMachineIdentifier, VZGenericPlatformConfiguration},
            socket_device::VZVirtioSocketDeviceConfiguration,
            virtual_machine::VZVirtualMachineConfigurationBuilder,
        };

        // Build a full config with new features (doesn't need real paths for the builder itself)
        let mut platform = VZGenericPlatformConfiguration::new();
        let machine_id = VZGenericMachineIdentifier::new();
        platform.set_machine_identifier(&machine_id);

        let vsock = VZVirtioSocketDeviceConfiguration::new();
        let entropy = VZVirtioEntropyDeviceConfiguration::new();
        let balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();

        // We can't fully build without valid paths for the boot loader,
        // but we CAN verify the builder accepts the new types
        let _builder = VZVirtualMachineConfigurationBuilder::new()
            .cpu_count(2)
            .memory_size(512 * 1024 * 1024)
            .platform(platform)
            .socket_devices(vec![vsock])
            .entropy_devices(vec![entropy])
            .memory_balloon_devices(vec![balloon]);
        // Build would fail validation without a boot loader, but the builder
        // accepting these types without crashing is the test
    }
}
