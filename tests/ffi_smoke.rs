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

        let restored = restored.expect("valid machine identifier bytes should roundtrip");
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

    // ─── VM Configuration Builder ────────────────────────────────────

    #[test]
    #[ignore = "requires virtualization entitlement (SIGSEGV without it)"]
    fn vm_config_builder_basic() {
        use vm_rs::ffi::apple_vz::{
            entropy_device::VZVirtioEntropyDeviceConfiguration,
            memory_device::VZVirtioTraditionalMemoryBalloonDeviceConfiguration,
            virtual_machine::VZVirtualMachineConfigurationBuilder,
        };

        let entropy = VZVirtioEntropyDeviceConfiguration::new();
        let balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();

        // Build a config with the types that exist
        let _builder = VZVirtualMachineConfigurationBuilder::new()
            .cpu_count(2)
            .memory_size(512 * 1024 * 1024)
            .entropy_devices(vec![entropy])
            .memory_balloon_devices(vec![balloon]);
        // Build would fail validation without a boot loader, but the builder
        // accepting these types without crashing is the test
    }
}
