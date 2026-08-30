import customtkinter as ctk
import threading
import time
import random
import numpy as np
from matplotlib.figure import Figure
from matplotlib.backends.backend_tkagg import FigureCanvasTkAgg
import serial.tools.list_ports
from benchvolt_remote import BenchVoltRemote
import struct
from tkinter import filedialog
import csv
import os

# ==========================================
# BOOTLOADER PROTOCOL CONSTANTS
# ==========================================
CMD_START = 0x01
CMD_DATA = 0x02
CMD_END = 0x03
CMD_JUMP_ONLY = 0x04
CMD_ERASE_CRC = 0x05
CMD_INFO = 0x10  # v2 boot-core identity probe

ACK = b'\x06'
NACK = b'\x15'
CHUNK_SIZE = 60

MAIN_APP_ADDR = 0x08008000
PARAM_PAGE_ADDR = 0x0801F800
V2_APP_ADDR = 0x08005000  # v2 boot chain app partition
V2_SEC_APP = 0  # sectioned-protocol section id: application


def calculate_stm32_crc32(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc = (crc ^ byte) & 0xFFFFFFFF
        for _ in range(8):
            if crc & 0x80000000:
                crc = ((crc << 1) ^ 0x04C11DB7) & 0xFFFFFFFF
            else:
                crc = (crc << 1) & 0xFFFFFFFF
    return crc


# --- UI Settings ---
ctk.set_appearance_mode("Dark")
ctk.set_default_color_theme("dark-blue")

# --- Waveform / ARB Settings ---
# Standard waveforms run on the firmware's built-in AWG engine, so the limits
# here are the same ones the on-device UI enforces: 125 Hz for square,
# 120 Hz for triangle/ramp/sine, 0.1 Hz minimum.
MIN_WAVE_FREQ_HZ = 0.1
MAX_WAVE_FREQ_HZ_SQUARE = 125
MAX_WAVE_FREQ_HZ_OTHER = 120
ARB_MAX_POINTS = 1024


class BenchVoltUI(ctk.CTk):
    def __init__(self):
        super().__init__()
        self.remote = BenchVoltRemote()

        self.current_arb_data = []  # Empty list initially
        self.arb_params = (1, 0)  # Default M=1, R=0
        self.fw_filepath = None  # Firmware file path
        self.syncing_outputs = False  # Prevent switch callbacks during MEAS:ALL? state sync
        self.output_sync_grace_until = [0, 0, 0, 0, 0]  # Prevent polling from fighting recent user switch commands
        self.syncing_setpoints = False  # Prevent slider callbacks during MEAS:ALL? Vset sync
        self.vset_sync_grace_until = [0, 0, 0, 0, 0]  # Prevent polling from fighting recent Vset commands
        self.current_limit_sync_grace_until = [0, 0, 0, 0, 0]  # Prevent polling from fighting recent current-limit commands
        self.current_limit_pending_values = [None, None, None, None, None]  # Hold user-entered current limit until firmware confirms it
        self.initial_vset_sync_done = False  # Sync CH4/CH5 Vset entry and slider only once after connection

        # Window Settings
        self.title("BenchVolt PD - Control Interface")
        self.geometry("1280x950")

        # Grid Layout
        self.grid_columnconfigure(1, weight=1)
        self.grid_rowconfigure(0, weight=1)

        # --- LEFT PANEL (Sidebar) ---
        self.setup_sidebar()

        # --- MAIN PANEL (TABVIEW) ---
        self.tabview = ctk.CTkTabview(self, fg_color="transparent")
        self.tabview.grid(row=0, column=1, sticky="nsew", padx=20, pady=20)

        self.tab_main = self.tabview.add("Main Control")
        self.tab_adv = self.tabview.add("Firmware Update")

        # Move existing main_scroll into tab_main
        self.main_scroll = ctk.CTkScrollableFrame(self.tab_main, fg_color="transparent")
        self.main_scroll.pack(fill="both", expand=True)
        self.main_scroll.grid_columnconfigure(0, weight=1)

        self.channels = []

        # SECTION 1: FIXED OUTPUTS
        self.create_section_header("Fixed Voltage Rails")
        self.fixed_frame = ctk.CTkFrame(self.main_scroll)
        self.fixed_frame.grid(row=1, column=0, sticky="ew", pady=(0, 30))
        self.fixed_frame.grid_columnconfigure((0, 1, 2), weight=1)

        self.channels.append(self.create_fixed_channel(self.fixed_frame, "CH1: 1.8V", 1.8, 0))
        self.channels.append(self.create_fixed_channel(self.fixed_frame, "CH2: 2.5V", 2.5, 1))
        self.channels.append(self.create_fixed_channel(self.fixed_frame, "CH3: 3.3V", 3.3, 2))

        # SECTION 2: ADJUSTABLE & WAVEFORM OUTPUTS
        self.create_section_header("Adjustable Rails & Waveform Generators")
        self.adj_container = ctk.CTkFrame(self.main_scroll, fg_color="transparent")
        self.adj_container.grid(row=3, column=0, sticky="ew")
        self.adj_container.grid_columnconfigure(0, weight=1)

        # Adj CH4 (0.5-5V)
        self.channels.append(
            self.create_adj_waveform_channel(self.adj_container, "CH4: Low Range (0.5V - 5V)", 0.5, 5.0, row=0))

        # Adj CH5 (0.8-22V)
        self.channels.append(
            self.create_adj_waveform_channel(self.adj_container, "CH5: High Range (0.8V - 22V)", 0.8, 22.0, row=1))

        # --- Firmware Update Tab Content ---
        # The raw DC-DC rail cards that used to live here spoke the archived
        # C firmware's command dialect (GET:DCDC:STAT, SOUR:VOLT:DC,
        # OUTP:DC); the Rust firmware deliberately has no raw-rail commands
        # (rails follow channel state through its staged executor), so the
        # tab is now just the firmware updater.
        adv_scroll = ctk.CTkScrollableFrame(self.tab_adv, fg_color="transparent")
        adv_scroll.pack(fill="both", expand=True)

        # --- FIRMWARE UPDATE SECTION (INTEGRATED BOOTLOADER) ---
        fw_frame = ctk.CTkFrame(adv_scroll, fg_color="#1a1a1a", border_width=2, border_color="#c0392b")
        fw_frame.pack(fill="x", padx=10, pady=30)

        ctk.CTkLabel(fw_frame, text="SYSTEM MAINTENANCE", font=ctk.CTkFont(size=14, weight="bold"),
                     text_color="#c0392b").pack(pady=(10, 5))

        # File Selection Frame
        file_frame = ctk.CTkFrame(fw_frame, fg_color="transparent")
        file_frame.pack(fill="x", padx=20, pady=5)

        self.fw_file_label = ctk.CTkLabel(file_frame, text="No file selected", font=ctk.CTkFont(size=12),
                                          text_color="#aaaaaa")
        self.fw_file_label.pack(side="left", padx=10)

        ctk.CTkButton(file_frame, text="Browse Firmware (.bin)", width=120, fg_color="#34495e", hover_color="#2c3e50",
                      command=self.browse_fw_file).pack(side="right", padx=10)

        # Progress Bar
        self.fw_progress = ctk.CTkProgressBar(fw_frame, progress_color="#3498db")
        self.fw_progress.pack(fill="x", padx=20, pady=(15, 5))
        self.fw_progress.set(0)

        # Log Textbox
        self.fw_log_box = ctk.CTkTextbox(fw_frame, height=150, fg_color="#0a0a0a", text_color="#2ecc71",
                                         font=ctk.CTkFont(family="Consolas", size=11))
        self.fw_log_box.pack(fill="x", padx=20, pady=5)
        self.fw_log_box.configure(state="disabled")

        # Update Button
        self.fw_btn = ctk.CTkButton(fw_frame,
                                    text="FIRMWARE UPDATE",
                                    font=ctk.CTkFont(size=14, weight="bold"),
                                    fg_color="#c0392b",
                                    hover_color="#a93226",
                                    height=40,
                                    command=self.start_fw_update_process)
        self.fw_btn.pack(pady=(10, 20), padx=40, fill="x")

        ctk.CTkLabel(fw_frame, text="Caution: Device will reboot automatically. Do not disconnect USB during update.",
                     font=ctk.CTkFont(size=10)).pack(pady=(0, 10))


        # Device controls stay locked until a verified BenchVolt is connected.
        self._set_device_controls_enabled(False)

    def browse_fw_file(self):
        filename = filedialog.askopenfilename(title="Select Firmware Binary",
                                              filetypes=(("Binary", "*.bin"), ("All files", "*.*")))
        if filename:
            self.fw_filepath = filename
            # Show only filename to avoid ugly long paths
            self.fw_file_label.configure(text=os.path.basename(filename), text_color="#2ecc71")

    def log_fw_msg(self, msg):
        """Thread-safe UI logger for firmware updates."""

        def append_log():
            self.fw_log_box.configure(state="normal")
            self.fw_log_box.insert("end", msg + "\n")
            self.fw_log_box.see("end")
            self.fw_log_box.configure(state="disabled")

        self.after(0, append_log)

    def set_fw_progress(self, val):
        """Thread-safe progress bar updater (0.0 to 1.0)."""
        self.after(0, lambda: self.fw_progress.set(val))

    def start_fw_update_process(self):
        port = self.port_option_menu.get()
        if port == "No Port Found" or not port:
            self.log_fw_msg("! ERROR: Please select a valid COM port.")
            return
        if not self._port_is_benchvolt(port):
            self.log_fw_msg(f"! ERROR: {port} is not a BenchVolt device")
            self.log_fw_msg("         (wrong port selected — refresh and pick the one")
            self.log_fw_msg("         whose USB identity is 0483:5740).")
            return
        if not self.fw_filepath or not os.path.exists(self.fw_filepath):
            self.log_fw_msg("! ERROR: Please select a valid firmware (.bin) file.")
            return

        # Lock UI
        self.fw_btn.configure(state="disabled", text="UPDATING...")
        self.fw_progress.set(0)

        # Clear Log
        self.fw_log_box.configure(state="normal")
        self.fw_log_box.delete("0.0", "end")
        self.fw_log_box.configure(state="disabled")

        # Start process in background. The generation token cancels any
        # still-running reconnect scan from a previous update, so it cannot
        # print into this run's log or probe ports mid-upload.
        self._fw_generation = getattr(self, "_fw_generation", 0) + 1
        threading.Thread(target=self.smart_upload_sequence, args=(port, self.fw_filepath), daemon=True).start()

    @staticmethod
    def _fw_image_target(firmware_data):
        """'v1' or 'v2' from the image's reset vector, None if neither."""
        if len(firmware_data) < 8:
            return None
        entry = struct.unpack_from("<I", firmware_data, 4)[0] & ~1
        if V2_APP_ADDR <= entry < MAIN_APP_ADDR:
            return "v2"
        if MAIN_APP_ADDR <= entry < 0x0801F000:
            return "v1"
        return None

    @staticmethod
    def _probe_v2(ser):
        """Identify a v2 device: 'core' (boot core), 'app' (v2 test app), or
        None (v1 bootloader / no v2 endpoint). The v1 bootloader NACKs or
        ignores the INFO byte, so the probe is harmless."""
        ser.reset_input_buffer()
        ser.write(bytes([CMD_INFO]))
        reply = ser.read(12)
        if reply[:4] == b"BV2C":
            return "core"
        if reply[:4] == b"BV2A":
            return "app"
        return None

    def _wait_for_port(self, port, timeout_s):
        """Wait for `port` to (re)appear; fall back to the first usbmodem-ish
        port that shows up. Returns the port path or None."""
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            available = [p.device for p in serial.tools.list_ports.comports()]
            if port in available:
                return port
            time.sleep(1)
        available = [p.device for p in serial.tools.list_ports.comports()]
        candidates = [p for p in available if "usbmodem" in p or "ttyACM" in p]
        return candidates[0] if candidates else None

    def _find_v2_core_port(self, timeout_s):
        """Scan serial ports and probe each for the v2 boot core identity;
        the core enumerates under a different name than the application."""
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            for candidate in serial.tools.list_ports.comports():
                device = candidate.device
                if "usbmodem" not in device and "ttyACM" not in device:
                    continue
                try:
                    with serial.Serial(device, 115200, timeout=1.5) as ser:
                        time.sleep(0.2)
                        if self._probe_v2(ser) == "core":
                            return device
                except Exception:
                    continue
            time.sleep(2)
        return None

    def _find_migrator(self, filepath):
        candidates = [
            os.path.join(os.path.dirname(filepath), "migrator.bin"),
            os.path.join(os.path.dirname(os.path.abspath(__file__)), "migrator.bin"),
            os.path.join(os.path.dirname(os.path.abspath(__file__)),
                         "..", "Firmware", "Rust", "boot-v2", "migrator.bin"),
        ]
        for candidate in candidates:
            if os.path.exists(candidate):
                return os.path.abspath(candidate)
        return None

    def smart_upload_sequence(self, port, filepath):
        try:
            self.log_fw_msg("==================================================")
            self.log_fw_msg("[SMART UPLOAD] Checking device state...")

            with open(filepath, "rb") as f:
                firmware_data = f.read()
            image_target = self._fw_image_target(firmware_data)
            if image_target is None:
                self.log_fw_msg("! ERROR: file is not a BenchVolt firmware image.")
                return
            self.log_fw_msg(f"-> Selected image targets the {image_target} boot layout.")

            in_app_mode = False

            # If currently connected in Python (User clicked "Connect" on UI)
            if self.remote.is_connected:
                in_app_mode = True
                self.log_fw_msg("-> Device is currently active in App Mode.")
                self.log_fw_msg("-> Sending JUMP:BOOTLOADER command...")
                self.remote.send_scpi("JUMP:BOOTLOADER")
            # Hard-stop the control session either way: toggle_conn would
            # RE-connect if the link had already dropped, and the polling
            # thread's try_reconnect grabs the port back (exclusively) the
            # moment the updater enumerates — which starved the post-update
            # reconnect scan and let stray SCPI hit the updater mid-flash.
            self.running = False
            self.remote.disconnect()

            def _ui_disconnected():
                self.status_indicator.configure(text="DISCONNECTED", text_color="red")
                self.connect_btn.configure(text="CONNECT (USB)", fg_color="#2ecc71")
                self._set_device_controls_enabled(False)

            self.after(0, _ui_disconnected)
            if not in_app_mode:
                # Not connected, but hardware might be stuck in app mode.
                # Settle after open, flush both sides, and lead with a
                # newline: a previous failed binary upload leaves junk in
                # the device's line buffer that would otherwise corrupt
                # this command (verified on the stock C firmware).
                try:
                    ser = serial.Serial(port, 115200, timeout=2)
                    time.sleep(0.4)
                    ser.reset_input_buffer()
                    ser.write(b"\nJUMP:BOOTLOADER\n")
                    res = ser.readline()
                    if not res:
                        ser.write(b"\nJUMP:BOOTLOADER\n")
                        res = ser.readline()
                    ser.close()
                    if b"OK:JUMPING" in res or b"BOOTLOADER" in res:
                        in_app_mode = True
                        self.log_fw_msg("-> Device detected in App Mode via raw serial!")
                        self.log_fw_msg("-> Bootloader transition triggered. Rebooting...")
                except Exception:
                    pass

            if in_app_mode:
                self.log_fw_msg("-> Waiting for the updater to re-enumerate...")
                time.sleep(2)

            # Identify which updater is on the wire. A v2 boot core comes up
            # under its OWN port name (different USB serial than the app or
            # the stock identity), so scan-and-probe every port for it FIRST;
            # only fall back to the selected port when no v2 core exists.
            mode = None
            core_port = self._find_v2_core_port(25 if in_app_mode else 5)
            if core_port:
                port = core_port
                mode = "core"
                self.log_fw_msg(f"-> v2 boot core found on {port}.")
            else:
                port = self._wait_for_port(port, 10) or port
                v1_confirmed = False
                try:
                    with serial.Serial(port, 115200, timeout=3) as ser:
                        time.sleep(0.3)
                        mode = self._probe_v2(ser)
                        if mode is None:
                            # Never assume "v1 legacy" from silence: demand a
                            # positive sign of the binary updater protocol.
                            # An orphan END is harmless and NACKed by both
                            # bootloader generations; silence means the port
                            # holds something else (an app mid-boot, a held
                            # port, nothing) and migrating would be wrong.
                            ser.reset_input_buffer()
                            ser.write(bytes([CMD_END, 0, 0, 0, 0]))
                            v1_confirmed = ser.read(1) == b"\x15"
                except Exception:
                    mode = None
                if mode is None and not v1_confirmed:
                    self.log_fw_msg("! ERROR: no updater answered on this port. The device")
                    self.log_fw_msg("         may still be booting or the port is busy —")
                    self.log_fw_msg("         wait a few seconds and press START again.")
                    return
            if mode == "app":
                self.log_fw_msg("! ERROR: a v2 application is running but refused "
                                "JUMP:BOOTLOADER. Reconnect and retry.")
                return
            self.log_fw_msg(f"-> Updater protocol: {'v2 sectioned' if mode == 'core' else 'v1 legacy'}.")

            if mode == "core" and image_target == "v2":
                if self.upload_firmware_v2(port, firmware_data):
                    self._reconnect_after_update()
            elif mode is None and image_target == "v1":
                self.upload_firmware(port, filepath)
            elif mode is None and image_target == "v2":
                self.migrate_then_upload(port, filepath, firmware_data)
            else:
                self.log_fw_msg(f"! ERROR: {image_target} image cannot be flashed over the "
                                f"{'v2' if mode == 'core' else 'v1'} protocol. Wrong file?")
        except Exception as e:
            self.log_fw_msg(f"! SMART UPLOAD ERROR: {e}")
        finally:
            self.after(0, lambda: self.fw_btn.configure(state="normal", text="START SMART UPDATE"))

    def migrate_then_upload(self, port, filepath, firmware_data):
        """One-time v1 -> v2 migration: flash the migrator through the stock
        bootloader, let it install the v2 boot chain, then flash the v2
        application over the sectioned protocol."""
        migrator = self._find_migrator(filepath)
        if migrator is None:
            self.log_fw_msg("! ERROR: this device still has the v1 bootloader and needs a")
            self.log_fw_msg("         one-time migration, but migrator.bin was not found")
            self.log_fw_msg("         next to the firmware file. Get it from the release.")
            return
        self.log_fw_msg("==================================================")
        self.log_fw_msg("[MIGRATION] Installing the v2 boot chain (one time).")
        self.log_fw_msg("  *** DO NOT UNPLUG OR POWER OFF THE DEVICE. ***")
        self.log_fw_msg("  Use a mains-powered supply, not a power bank.")
        if not self.upload_firmware(port, migrator):
            self.log_fw_msg("! MIGRATION ABORTED: migrator upload failed; device unchanged.")
            return
        self.log_fw_msg("[MIGRATION] Migrator flashed; the device is rewriting its")
        self.log_fw_msg("            boot pages and will reboot. Waiting...")
        time.sleep(8)
        # The v2 core enumerates under a NEW port name (different USB
        # serial string), so scan every candidate port and probe each for
        # the v2 identity instead of waiting for the old name.
        port = self._find_v2_core_port(45)
        if port is None:
            self.log_fw_msg("! ERROR: v2 boot core did not answer after migration.")
            self.log_fw_msg("         Refresh ports, select the new one, and retry —")
            self.log_fw_msg("         the update resumes from where it left off.")
            return
        self.log_fw_msg(f"-> v2 boot core found on {port}.")
        self.log_fw_msg("[MIGRATION] v2 boot core is up. Flashing the application...")
        if self.upload_firmware_v2(port, firmware_data):
            self._reconnect_after_update()

    def _reconnect_after_update(self):
        """After a successful update + reboot, find the running firmware and
        reconnect the control session automatically. Exits silently if a
        newer smart update has started meanwhile."""
        generation = getattr(self, "_fw_generation", 0)
        self.log_fw_msg("[RECONNECT] Waiting for the new firmware to boot...")
        deadline = time.time() + 30
        while time.time() < deadline:
            if getattr(self, "_fw_generation", 0) != generation:
                return
            for candidate in serial.tools.list_ports.comports():
                if (candidate.vid, candidate.pid) != self.BENCHVOLT_USB_ID:
                    continue
                try:
                    with serial.Serial(candidate.device, 115200, timeout=1.5) as ser:
                        time.sleep(0.3)
                        ser.reset_input_buffer()
                        ser.write(b"\n*IDN?\n")
                        if b"BenchVolt" in ser.readline():
                            device = candidate.device
                            self.log_fw_msg(f"[RECONNECT] Firmware is up on {device}. Connecting.")
                            self.after(0, lambda d=device: (self.port_option_menu.set(d),
                                                            self.toggle_conn()))
                            return
                except Exception:
                    continue
            time.sleep(2)
        if getattr(self, "_fw_generation", 0) == generation:
            self.log_fw_msg("[RECONNECT] Firmware did not answer within 30 s; connect manually.")

    def upload_firmware_v2(self, port, firmware_data):
        """Sectioned v2 upload: START carries the section id, END commits the
        in-partition descriptor, and a BOOT command launches the new image."""
        try:
            file_size = len(firmware_data)
            calculated_crc = calculate_stm32_crc32(firmware_data)
            self.log_fw_msg("--------------------------------------------------")
            self.log_fw_msg("[V2 STEP 1] PREPARATION")
            self.log_fw_msg(f"         File Size    : {file_size} Bytes")
            self.log_fw_msg(f"         Target Addr  : {hex(V2_APP_ADDR)}")
            self.log_fw_msg(f"         Python CRC32 : {hex(calculated_crc)}")

            ser = serial.Serial(port, 115200, timeout=15)
            time.sleep(0.3)

            self.log_fw_msg("[V2 STEP 2] ERASING APPLICATION SECTION")
            ser.write(struct.pack("<BIB", CMD_START, file_size, V2_SEC_APP))
            if ser.read(1) != ACK:
                self.log_fw_msg("! ERROR: no ACK for START/erase. Aborting.")
                ser.close()
                return
            self.log_fw_msg("         -> ACK. Section erased.")

            self.log_fw_msg("[V2 STEP 3] WRITING DATA")
            sent_bytes = 0
            for i in range(0, file_size, CHUNK_SIZE):
                chunk = firmware_data[i:i + CHUNK_SIZE]
                ser.write(struct.pack("<BH", CMD_DATA, len(chunk)) + chunk)
                if ser.read(1) != ACK:
                    self.log_fw_msg(f"! ERROR: ACK failure at offset {sent_bytes}.")
                    ser.close()
                    return
                sent_bytes += len(chunk)
                self.set_fw_progress(sent_bytes / file_size)

            self.log_fw_msg("[V2 STEP 4] VERIFY & COMMIT (descriptor last)")
            ser.write(struct.pack("<BI", CMD_END, calculated_crc))
            if ser.read(1) != ACK:
                self.log_fw_msg("! ERROR: CRC verification failed; device stays in updater.")
                ser.close()
                self.set_fw_progress(0.0)
                return
            self.log_fw_msg("         -> Committed. CRC verified, descriptor sealed.")

            self.log_fw_msg("[V2 STEP 5] REBOOTING INTO THE NEW FIRMWARE")
            ser.write(bytes([CMD_JUMP_ONLY]))
            rebooted = ser.read(1) == ACK
            if rebooted:
                self.log_fw_msg("         -> Device is rebooting. Update complete!")
                self.set_fw_progress(1.0)
            else:
                self.log_fw_msg("         -> No reboot ACK; power-cycle the device.")
            ser.close()
            return rebooted
        except Exception as e:
            self.log_fw_msg(f"! CRITICAL V2 UPLOAD ERROR: {e}")
            return False

    def upload_firmware(self, port, filepath):
        try:
            with open(filepath, "rb") as f:
                firmware_data = f.read()

            file_size = len(firmware_data)
            calculated_crc = calculate_stm32_crc32(firmware_data)

            self.log_fw_msg("--------------------------------------------------")
            self.log_fw_msg(f"[STEP 1] PREPARATION & ANALYSIS")
            self.log_fw_msg(f"         File Size    : {file_size} Bytes")
            self.log_fw_msg(f"         Target Addr  : {hex(MAIN_APP_ADDR)}")
            self.log_fw_msg(f"         Python CRC32 : {hex(calculated_crc)}")

            ser = serial.Serial(port, 115200, timeout=15)
            time.sleep(0.5)

            self.log_fw_msg("\n[STEP 2] ERASING APP AREA (ERASE STAGE)")
            self.log_fw_msg(f"         Sending erase command from addr {hex(MAIN_APP_ADDR)}...")
            ser.write(struct.pack("<BI", CMD_START, file_size))

            if ser.read(1) != ACK:
                self.log_fw_msg("! ERROR: Failed to receive ACK for erase. Aborting.")
                ser.close()
                self.after(0, lambda: self.fw_btn.configure(state="normal", text="START SMART UPDATE"))
                return
            self.log_fw_msg("         -> ACK Received! Flash erased, area is clean.")

            self.log_fw_msg("\n[STEP 3] WRITING DATA (WRITE STAGE)")
            sent_bytes = 0
            for i in range(0, file_size, CHUNK_SIZE):
                chunk = firmware_data[i:i + CHUNK_SIZE]
                chunk_len = len(chunk)
                current_write_addr = MAIN_APP_ADDR + sent_bytes

                ser.write(struct.pack("<BH", CMD_DATA, chunk_len) + chunk)
                if ser.read(1) != ACK:
                    self.log_fw_msg(f"! ERROR: ACK failure while writing to {hex(current_write_addr)}!");
                    ser.close()
                    self.after(0, lambda: self.fw_btn.configure(state="normal", text="START SMART UPDATE"))
                    return

                sent_bytes += chunk_len
                progress_val = sent_bytes / file_size

                if (i // CHUNK_SIZE) % 10 == 0 or sent_bytes == file_size:
                    self.log_fw_msg(
                        f"         Writing -> Addr: {hex(current_write_addr)} | {progress_val * 100:.1f}% Completed")

                self.set_fw_progress(progress_val)

            self.log_fw_msg("\n[STEP 4] VERIFICATION & SEALING")
            self.log_fw_msg("         Sending verification (END) command...")
            self.log_fw_msg(f"         MCU will calculate CRC and compare with {hex(calculated_crc)}.")
            ser.write(struct.pack("<BI", CMD_END, calculated_crc))

            response = ser.read(1)
            if response == ACK:
                self.log_fw_msg("         -> VERIFICATION SUCCESSFUL! CRC matches.")
                self.log_fw_msg(f"         -> SEAL APPLIED! {hex(PARAM_PAGE_ADDR)} erased & new CRC sealed.")
                self.log_fw_msg("\n[STEP 5] COMPLETE - JUMPING TO APP!")
                self.set_fw_progress(1.0)
                ser.close()
                return True
            else:
                self.log_fw_msg("! ERROR: VERIFICATION FAILED!")
                self.log_fw_msg("         MCU CRC does not match Python CRC.")
                self.log_fw_msg("         Sealing ABORTED. Device remains in bootloader.")
                self.set_fw_progress(0.0)

            ser.close()
        except Exception as e:
            self.log_fw_msg(f"! CRITICAL UPLOAD ERROR: {e}")
        finally:
            self.after(0, lambda: self.fw_btn.configure(state="normal", text="START SMART UPDATE"))

    def get_serial_ports(self):
        """Returns a list of available serial ports."""
        ports = serial.tools.list_ports.comports()
        return [port.device for port in ports] if ports else ["No Port Found"]

    def setup_sidebar(self):
        self.sidebar = ctk.CTkFrame(self, width=250, corner_radius=0)
        self.sidebar.grid(row=0, column=0, sticky="nsew")

        lbl = ctk.CTkLabel(self.sidebar, text="BENCHVOLT PD\nControl System", font=ctk.CTkFont(size=20, weight="bold"))
        lbl.pack(pady=(40, 40))

        # Port Selection Section
        port_frame = ctk.CTkFrame(self.sidebar)
        port_frame.pack(fill="x", padx=20, pady=10)

        ctk.CTkLabel(port_frame, text="Select Port:", font=ctk.CTkFont(weight="bold")).pack(anchor="w", padx=10,
                                                                                            pady=(10, 0))

        # Port Dropdown
        self.port_option_menu = ctk.CTkOptionMenu(port_frame, values=self.get_serial_ports())
        self.port_option_menu.pack(fill="x", padx=10, pady=5)

        # Refresh Ports Button
        self.refresh_ports_btn = ctk.CTkButton(port_frame, text="Refresh Ports",
                                               font=ctk.CTkFont(size=11),
                                               height=20,
                                               fg_color="#34495e",
                                               command=lambda: self.port_option_menu.configure(
                                                   values=self.get_serial_ports()))
        self.refresh_ports_btn.pack(fill="x", padx=10, pady=(0, 10))

        # Connection Status
        status_frame = ctk.CTkFrame(self.sidebar)
        status_frame.pack(fill="x", padx=20, pady=10)
        ctk.CTkLabel(status_frame, text="Connection Status:", font=ctk.CTkFont(weight="bold")).pack(anchor="w", padx=10,
                                                                                                    pady=(10, 0))
        self.status_indicator = ctk.CTkLabel(status_frame, text="DISCONNECTED", text_color="red",
                                             font=ctk.CTkFont(size=12))
        self.status_indicator.pack(anchor="w", padx=10, pady=(5, 10))

        self.connect_btn = ctk.CTkButton(self.sidebar, text="CONNECT (USB)", fg_color="#2ecc71",
                                         command=self.toggle_conn)
        self.connect_btn.pack(padx=20, pady=10)

        # Input Power Info & PDO Selection Section
        info_frame = ctk.CTkFrame(self.sidebar)
        info_frame.pack(fill="x", padx=20, pady=10)

        ctk.CTkLabel(info_frame, text="PD Input Status:", font=ctk.CTkFont(weight="bold")).pack(anchor="w", padx=10,
                                                                                                pady=(10, 0))

        self.pd_info = ctk.CTkLabel(info_frame, text="N/A", font=ctk.CTkFont(size=16), text_color="#3498db")
        self.pd_info.pack(anchor="w", padx=10, pady=(5, 10))

        # PDO Selection Dropdown (Empty initially)
        self.pdo_menu = ctk.CTkOptionMenu(info_frame, values=["No PDOs Found"])
        self.pdo_menu.pack(fill="x", padx=10, pady=5)

        # Fetch List Button
        self.get_pdo_btn = ctk.CTkButton(info_frame, text="Get PDO List",
                                         height=25, fg_color="#34495e",
                                         command=self.update_pdo_dropdown)
        self.get_pdo_btn.pack(fill="x", padx=10, pady=5)

        # Activate Button
        self.set_pdo_btn = ctk.CTkButton(info_frame, text="Set Active PDO",
                                         height=30, fg_color="#d35400",
                                         command=self.set_selected_pdo)
        self.set_pdo_btn.pack(fill="x", padx=10, pady=(5, 15))

        # --- NEWLY ADDED: MANUAL PDO OVERRIDE SECTION ---
        manual_frame = ctk.CTkFrame(self.sidebar)
        manual_frame.pack(fill="x", padx=20, pady=10)

        ctk.CTkLabel(manual_frame, text="Manual PDO Override:", font=ctk.CTkFont(weight="bold")).pack(anchor="w",
                                                                                                      padx=10,
                                                                                                      pady=(10, 5))

        # mV and mA Text Boxes
        entry_frame = ctk.CTkFrame(manual_frame, fg_color="transparent")
        entry_frame.pack(fill="x", padx=10, pady=5)

        ctk.CTkLabel(entry_frame, text="mV:", font=ctk.CTkFont(size=12)).grid(row=0, column=0, padx=(0, 2))
        self.man_v_entry = ctk.CTkEntry(entry_frame, width=50, height=25)
        self.man_v_entry.insert(0, "20000")
        self.man_v_entry.grid(row=0, column=1, padx=(0, 10))

        ctk.CTkLabel(entry_frame, text="mA:", font=ctk.CTkFont(size=12)).grid(row=0, column=2, padx=(0, 2))
        self.man_i_entry = ctk.CTkEntry(entry_frame, width=50, height=25)
        self.man_i_entry.insert(0, "3000")
        self.man_i_entry.grid(row=0, column=3)

        # Quick Selection Buttons (Aligned and Small Grid)
        btn_frame = ctk.CTkFrame(manual_frame, fg_color="transparent")
        btn_frame.pack(fill="x", padx=10, pady=(2, 10))

        # Row 1: Voltage Buttons
        v_values = [5, 9, 12, 15, 20]
        for col, v in enumerate(v_values):
            btn = ctk.CTkButton(btn_frame, text=f"{v}V", width=35, height=20, font=ctk.CTkFont(size=10),
                                command=lambda val=v: (
                                    self.man_v_entry.delete(0, "end"), self.man_v_entry.insert(0, str(val * 1000))))
            btn.grid(row=0, column=col, padx=2, pady=2)

        # Row 2: Current Buttons
        i_values = [1, 2, 3, 4, 5]
        for col, i in enumerate(i_values):
            btn = ctk.CTkButton(btn_frame, text=f"{i}A", width=35, height=20, font=ctk.CTkFont(size=10),
                                command=lambda val=i: (
                                    self.man_i_entry.delete(0, "end"), self.man_i_entry.insert(0, str(val * 1000))))
            btn.grid(row=1, column=col, padx=2, pady=2)

        # Manual Send Button
        self.send_man_btn = ctk.CTkButton(manual_frame, text="Send Manual PDO", height=28, fg_color="#8e44ad",
                                          hover_color="#732d91", command=self.send_manual_pdo)
        self.send_man_btn.pack(fill="x", padx=10, pady=(5, 15))
        # -----------------------------------------------

        # --- System & Firmware Info ---
        sys_frame = ctk.CTkFrame(self.sidebar)
        sys_frame.pack(fill="x", padx=20, pady=10)

        # 1. Temp (sticky="w")
        ctk.CTkLabel(sys_frame, text="Sys Temp:", font=ctk.CTkFont(weight="bold")).grid(row=0, column=0, padx=(10, 5),
                                                                                        pady=(10, 5), sticky="w")
        self.temp_info = ctk.CTkLabel(sys_frame, text="N/A", font=ctk.CTkFont(size=14, weight="bold"),
                                      text_color="#e67e22")
        self.temp_info.grid(row=0, column=1, padx=(0, 10), pady=(10, 5), sticky="w")

        # 2. Firmware Date
        ctk.CTkLabel(sys_frame, text="FW Date:", font=ctk.CTkFont(weight="bold")).grid(row=1, column=0, padx=(10, 5),
                                                                                       pady=(0, 10), sticky="w")
        self.fw_date_info = ctk.CTkLabel(sys_frame, text="--", font=ctk.CTkFont(size=11), text_color="#2ecc71")
        self.fw_date_info.grid(row=1, column=1, padx=(0, 10), pady=(0, 10), sticky="w")

    def create_section_header(self, text):
        label = ctk.CTkLabel(self.main_scroll, text=text, font=ctk.CTkFont(size=18, weight="bold"), anchor="w")
        label.grid(sticky="w", pady=(10, 5))
        separator = ctk.CTkProgressBar(self.main_scroll, height=2, progress_color="gray")
        separator.set(1)
        separator.grid(sticky="ew", pady=(0, 20))

    def set_channel_current_limit(self, channel_number, entry):
        try:
            limit = float(entry.get())
        except ValueError:
            entry.configure(text_color="#e74c3c")
            return

        if limit < 0.0:
            limit = 0.0
        if limit > 3.0:
            limit = 3.0

        entry.delete(0, "end")
        entry.insert(0, f"{limit:.2f}")
        entry.configure(text_color="#2ecc71")

        self.current_limit_pending_values[channel_number - 1] = limit
        self.current_limit_sync_grace_until[channel_number - 1] = time.time() + 3.0

        if self.remote.is_connected:
            self.remote.send_scpi(f"SOUR:CURR:CH{channel_number} {limit:.2f}")

    def create_fixed_channel(self, parent, name, voltage, col):
        """
        Creates a fixed voltage channel UI block and links it to hardware control.
        """
        frame = ctk.CTkFrame(parent)
        frame.grid(row=0, column=col, padx=10, pady=10, sticky="nsew")

        def validate_numeric(P):
            if P == "" or P == ".": return True
            try:
                float(P)
                return True
            except ValueError:
                return False

        curr_cmd = (self.register(validate_numeric), '%P')

        ctk.CTkLabel(frame, text=name, font=ctk.CTkFont(size=16, weight="bold")).pack(pady=(15, 5))

        # --- Nested Function for Event Handling ---
        def on_toggle():
            # 1. Determine the state (1 for ON, 0 for OFF)
            state = 1 if switch.get() else 0

            # 2. Update the UI Switch text
            switch.configure(text="ON" if state else "OFF")

            if getattr(self, 'syncing_outputs', False):
                return

            # 3. Send the SCPI command to the hardware
            # Mapping: col 0 -> CH1, col 1 -> CH2, col 2 -> CH3
            channel_number = col + 1
            self.output_sync_grace_until[channel_number - 1] = time.time() + 1.0
            self.remote.set_output(channel_number, state)

        # Create the switch
        switch = ctk.CTkSwitch(frame, text="OFF", font=ctk.CTkFont(weight="bold"))

        # Link the switch to the nested function
        switch.configure(command=on_toggle)
        switch.pack(pady=10)

        # Readout display
        readout = ctk.CTkFrame(frame, fg_color="#2b2b2b")
        readout.pack(fill="x", padx=15, pady=15)

        v_lbl = ctk.CTkLabel(readout, text=f"{voltage:.1f} V",
                             font=ctk.CTkFont(size=22, weight="bold"),
                             text_color="#4cb5f5")
        v_lbl.pack(side="left", padx=10)

        i_lbl = ctk.CTkLabel(readout, text="0.000 A",
                             font=ctk.CTkFont(size=22, weight="bold"),
                             text_color="#f39c12")
        i_lbl.pack(side="right", padx=10)

        ilim_lbl = ctk.CTkLabel(readout, text="Lim: -- A",
                                font=ctk.CTkFont(size=13, weight="bold"),
                                text_color="#2ecc71")
        ilim_lbl.pack(side="right", padx=10)

        curr_frame = ctk.CTkFrame(frame, fg_color="transparent")
        curr_frame.pack(fill="x", padx=15, pady=(0, 15))
        ctk.CTkLabel(curr_frame, text="Current Limit (A):", font=ctk.CTkFont(size=11)).pack(side="left", padx=(0, 5))
        curr_entry = ctk.CTkEntry(curr_frame, width=70, validate="key", validatecommand=curr_cmd)
        curr_entry.insert(0, "3.00")
        curr_entry.pack(side="left", padx=(0, 5))
        curr_btn = ctk.CTkButton(curr_frame, text="Set", width=45, height=24,
                                 fg_color="#27ae60", hover_color="#219150",
                                 command=lambda: self.set_channel_current_limit(col + 1, curr_entry))
        curr_btn.pack(side="left")
        curr_entry.bind("<Return>", lambda e: self.set_channel_current_limit(col + 1, curr_entry))

        return {"type": "fixed", "switch": switch, "i_lbl": i_lbl, "ilim_lbl": ilim_lbl, "ilim_entry": curr_entry}

    def create_adj_waveform_channel(self, parent, name, min_v, max_v, row):
        main_card = ctk.CTkFrame(parent, border_width=2, border_color="#444444")
        main_card.grid(row=row, column=0, sticky="ew", pady=10)
        main_card.grid_columnconfigure(0, weight=1)

        # --- INPUT VALIDATION ---
        def validate_numeric(P):
            if P == "" or P == ".": return True  # Allow empty or just a dot
            try:
                float(P)  # Allow if numeric format (No limit check here)
                return True
            except ValueError:
                return False

        # Simplify v_cmd and f_cmd
        v_cmd = (self.register(validate_numeric), '%P')
        f_cmd = (self.register(validate_numeric), '%P')

        # Top Section
        top_frame = ctk.CTkFrame(main_card, fg_color="transparent")
        top_frame.pack(fill="x", padx=20, pady=20)
        ctk.CTkLabel(top_frame, text=name, font=ctk.CTkFont(size=18, weight="bold")).pack(side="left")

        mode_switch = ctk.CTkSegmentedButton(top_frame, values=["DC (Steady)", "Waveform Gen."])
        mode_switch.set("DC (Steady)")
        mode_switch.pack(side="right")

        # DC Slider
        slider_container = ctk.CTkFrame(main_card)
        slider_container.pack(fill="x", padx=20, pady=(0, 20))
        ctk.CTkLabel(slider_container, text="DC Voltage Set:").pack(anchor="w", padx=10, pady=(10, 0))

        slider = ctk.CTkSlider(slider_container, from_=min_v, to=max_v, number_of_steps=100, height=25)
        slider.pack(fill="x", padx=10, pady=10)
        slider.set(min_v)

        # Bottom Controls
        bottom_ctrl = ctk.CTkFrame(main_card, fg_color="transparent")
        bottom_ctrl.pack(fill="x", padx=20, pady=(0, 20))
        master_switch = ctk.CTkSwitch(bottom_ctrl, text="OUTPUT OFF", font=ctk.CTkFont(size=14, weight="bold"),
                                      height=30)
        master_switch.pack(side="left")

        def toggle_output():
            state = 1 if master_switch.get() else 0
            ch_num = 4 if row == 0 else 5
            master_switch.configure(text="OUTPUT ON" if state else "OUTPUT OFF")
            if getattr(self, 'syncing_outputs', False):
                return
            if mode_switch.get() == "Waveform Gen.":
                # In waveform mode the master switch starts/stops the
                # generator itself; a start can take a few seconds to
                # sequence, so widen the poll-sync grace window.
                self.output_sync_grace_until[ch_num - 1] = time.time() + 4.0
                if state:
                    send_final_data()
                else:
                    stop_waveform()
            else:
                self.output_sync_grace_until[ch_num - 1] = time.time() + 1.0
                # Send command to hardware
                self.remote.set_output(ch_num, state)

        master_switch.configure(command=toggle_output)

        # READOUT AREA
        readout = ctk.CTkFrame(bottom_ctrl, fg_color="#2b2b2b", height=50)
        readout.pack(side="right")
        v_entry = ctk.CTkEntry(readout, width=90, font=ctk.CTkFont(size=24, weight="bold"), text_color="#4cb5f5",
                               fg_color="transparent", border_width=0, validate="key", validatecommand=v_cmd)
        v_entry.insert(0, f"{min_v:.2f}")
        v_entry.pack(side="left", padx=(10, 0))

        # --- NEWLY ADDED VSET LOGIC (Enter and Color Control) ---
        def apply_manual_vset(event=None):
            try:
                val = float(v_entry.get())
                if min_v <= val <= max_v:
                    v_entry.configure(text_color="#4cb5f5")  # Blue if valid
                    self.syncing_setpoints = True
                    try:
                        slider.set(val)  # Align slider to new value
                    finally:
                        self.syncing_setpoints = False
                    ch_num = 4 if row == 0 else 5
                    self.vset_sync_grace_until[ch_num - 1] = time.time() + 1.0
                    self.remote.set_voltage(ch_num, val)  # Send to device

                    # Beautify format (e.g. 5 to 5.00)
                    v_entry.delete(0, "end")
                    v_entry.insert(0, f"{val:.2f}")
                    readout.focus_set()  # Remove cursor from box on Enter (Focus out)
                else:
                    v_entry.configure(text_color="#e74c3c")  # Red if out of bounds
            except ValueError:
                v_entry.configure(text_color="#e74c3c")

        def check_vset_bounds(event=None):
            try:
                val = float(v_entry.get())
                if min_v <= val <= max_v:
                    v_entry.configure(text_color="#4cb5f5")
                else:
                    v_entry.configure(text_color="#e74c3c")
            except ValueError:
                if v_entry.get() not in ["", "."]:
                    v_entry.configure(text_color="#e74c3c")

        v_entry.bind("<Return>", apply_manual_vset)
        v_entry.bind("<KeyRelease>", check_vset_bounds)
        # --------------------------------------------------------

        ctk.CTkLabel(readout, text="Vset", font=ctk.CTkFont(size=12, weight="bold"), text_color="#4cb5f5").pack(
            side="left", padx=(0, 10))
        v_read_lbl = ctk.CTkLabel(readout, text="0.00 V", font=ctk.CTkFont(size=24, weight="bold"),
                                  text_color="#2ecc71")
        v_read_lbl.pack(side="left", padx=10)
        i_lbl = ctk.CTkLabel(readout, text="0.000 A", font=ctk.CTkFont(size=24, weight="bold"), text_color="#f39c12")
        i_lbl.pack(side="right", padx=20)

        ilim_lbl = ctk.CTkLabel(readout, text="Lim: -- A", font=ctk.CTkFont(size=13, weight="bold"), text_color="#2ecc71")
        ilim_lbl.pack(side="right", padx=10)

        curr_limit_frame = ctk.CTkFrame(main_card, fg_color="transparent")
        curr_limit_frame.pack(fill="x", padx=20, pady=(0, 15))
        ctk.CTkLabel(curr_limit_frame, text="Current Limit (A):", font=ctk.CTkFont(size=11)).pack(side="left", padx=(0, 5))
        curr_entry = ctk.CTkEntry(curr_limit_frame, width=70, validate="key", validatecommand=v_cmd)
        curr_entry.insert(0, "3.00")
        curr_entry.pack(side="left", padx=(0, 5))
        curr_btn = ctk.CTkButton(curr_limit_frame, text="Set", width=45, height=24,
                                 fg_color="#27ae60", hover_color="#219150",
                                 command=lambda: self.set_channel_current_limit(4 if row == 0 else 5, curr_entry))
        curr_btn.pack(side="left")
        curr_entry.bind("<Return>", lambda e: self.set_channel_current_limit(4 if row == 0 else 5, curr_entry))

        # --- WAVEFORM PANEL ---
        wave_panel = ctk.CTkFrame(main_card, fg_color="#222222")
        wave_panel.grid_columnconfigure(1, weight=1)

        wave_ctrls = ctk.CTkFrame(wave_panel, fg_color="transparent")
        wave_ctrls.grid(row=0, column=0, sticky="ns", padx=20, pady=20)

        wave_type = ctk.CTkOptionMenu(wave_ctrls, values=["Square", "Triangle", "Ramp", "Sine", "Custom ARB"],
                                      width=150)
        wave_type.pack(pady=5)

        freq_label = ctk.CTkLabel(wave_ctrls, text=f"Frequency ({MIN_WAVE_FREQ_HZ}-{MAX_WAVE_FREQ_HZ_SQUARE} Hz):")
        freq_label.pack(anchor="w", pady=(5, 0))
        freq_entry = ctk.CTkEntry(wave_ctrls, width=150, validate="key", validatecommand=f_cmd)
        freq_entry.insert(0, "100")
        freq_entry.pack(pady=5)

        duty_label = ctk.CTkLabel(wave_ctrls, text="Duty Cycle (1-99 %):")
        duty_label.pack(anchor="w", pady=(5, 0))
        duty_entry = ctk.CTkEntry(wave_ctrls, width=150, validate="key", validatecommand=f_cmd)
        duty_entry.insert(0, "50")
        duty_entry.pack(pady=5)

        limit_frame = ctk.CTkFrame(wave_ctrls, fg_color="transparent")
        limit_frame.pack(fill="x", pady=5)

        v_min_box = ctk.CTkFrame(limit_frame, fg_color="transparent")
        v_min_box.pack(side="left")
        ctk.CTkLabel(v_min_box, text="V-Min:", font=ctk.CTkFont(size=11)).pack(anchor="w")
        w_vmin_entry = ctk.CTkEntry(v_min_box, width=70, validate="key", validatecommand=v_cmd)
        w_vmin_entry.insert(0, f"{min_v:.2f}")
        w_vmin_entry.pack(side="left", padx=(0, 5))

        v_max_box = ctk.CTkFrame(limit_frame, fg_color="transparent")
        v_max_box.pack(side="left")
        ctk.CTkLabel(v_max_box, text="V-Max:", font=ctk.CTkFont(size=11)).pack(anchor="w")
        w_vmax_entry = ctk.CTkEntry(v_max_box, width=70, validate="key", validatecommand=v_cmd)
        w_vmax_entry.insert(0, f"{max_v:.2f}")
        w_vmax_entry.pack(side="left")

        # Store channel-specific inputs
        local_wave_inputs = [freq_entry, duty_entry, w_vmin_entry, w_vmax_entry]

        # --- ARB SPECIAL AREA ---
        arb_frame = ctk.CTkFrame(wave_ctrls, fg_color="transparent")
        load_btn = ctk.CTkButton(arb_frame, text="📁 LOAD CSV FILE", fg_color="#34495e", command=lambda: load_csv_file())
        load_btn.pack(fill="x", pady=10)
        file_info_label = ctk.CTkLabel(arb_frame, text="No file loaded", font=ctk.CTkFont(size=10),
                                       text_color="#777777")
        file_info_label.pack(anchor="w")

        # Store labels in class dictionary to access them during chunk upload
        if not hasattr(self, 'arb_status_labels'):
            self.arb_status_labels = {}
        self.arb_status_labels[4 if row == 0 else 5] = file_info_label

        apply_btn = ctk.CTkButton(wave_panel, text="🚀 SEND ", fg_color="#27ae60", hover_color="#219150",
                                  font=ctk.CTkFont(weight="bold"))
        apply_btn.grid(row=1, column=0, pady=(20, 0), padx=(20, 5), sticky="ew")
        stop_btn = ctk.CTkButton(wave_panel, text="⏹ STOP", fg_color="#c0392b", hover_color="#992d22",
                                 font=ctk.CTkFont(weight="bold"))
        stop_btn.grid(row=1, column=1, pady=(20, 0), padx=(5, 20), sticky="ew")

        # Always-visible engine status line (the ARB file label lives inside
        # the ARB frame, which is hidden for standard waveforms).
        wave_status_label = ctk.CTkLabel(wave_panel, text="", font=ctk.CTkFont(size=12),
                                         text_color="#777777")
        wave_status_label.grid(row=2, column=0, columnspan=2, pady=(4, 12), padx=20, sticky="w")
        if not hasattr(self, 'wave_status_labels'):
            self.wave_status_labels = {}
        self.wave_status_labels[4 if row == 0 else 5] = wave_status_label

        # Graph Area
        graph_frame = ctk.CTkFrame(wave_panel)
        graph_frame.grid(row=0, column=1, sticky="nsew", padx=20, pady=20)
        fig = Figure(figsize=(4, 2.5), dpi=100)
        fig.patch.set_facecolor('#222222')
        ax = fig.add_subplot(111)
        canvas = FigureCanvasTkAgg(fig, master=graph_frame)
        canvas.get_tk_widget().pack(fill="both", expand=True)

        # --- FUNCTIONS ---
        def load_csv_file():
            file_path = filedialog.askopenfilename(filetypes=[("CSV files", "*.csv")])
            if file_path:
                try:
                    new_points = []
                    m_val, r_val = 1, 0  # Default values

                    with open(file_path, 'r', encoding='utf-8') as f:
                        lines = f.readlines()

                    for i, line in enumerate(lines):
                        line = line.strip()
                        if not line or "Voltage_DAC" in line:
                            continue

                        # 1. Header Check (#CONF:MULTIPLIER=1,REPETITION=3)
                        if line.startswith('#CONF:'):
                            parts = line.replace('#CONF:', '').split(',')
                            for part in parts:
                                if 'MULTIPLIER' in part:
                                    parsed_multiplier = float(part.split('=')[-1])
                                    if parsed_multiplier != 0.5 and (
                                        parsed_multiplier < 1 or not parsed_multiplier.is_integer()
                                    ):
                                        raise ValueError("Multiplier must be 0.5 or a positive integer")
                                    m_val = int(parsed_multiplier) if parsed_multiplier.is_integer() else 0.5
                                if 'REPETITION' in part:
                                    r_val = int(part.split('=')[-1])
                            continue

                        # 2. Data Reading and Limit Check
                        parts = line.split(',')
                        if len(parts) >= 2:
                            v_val = float(parts[0])
                            d_val = int(parts[1])

                            if not (1 <= d_val <= 65535):
                                raise ValueError(f"Row {i + 1} dwell must be 1..65535")

                            # CHANNEL-BASED VOLTAGE CONTROL
                            if not (min_v <= v_val <= max_v):
                                print(
                                    f"❌ ERROR: Row {i + 1} voltage {v_val}V is out of range for this channel ({min_v}V - {max_v}V)!")
                                file_info_label.configure(text=f"❌ Range Error: {v_val}V", text_color="#e74c3c")
                                return  # Stop loading on invalid data

                            new_points.append((v_val, d_val))
                            if len(new_points) > ARB_MAX_POINTS:
                                raise ValueError(f"ARB exceeds {ARB_MAX_POINTS} points")

                    if new_points:
                        self.current_arb_data = new_points
                        self.arb_params = (m_val, r_val)
                        file_info_label.configure(text=f"✔ {len(new_points)} Pts | M:{m_val} | R:{r_val}",
                                                  text_color="#2ecc71")
                        update_plot()
                        print(f"SUCCESS: Loaded {len(new_points)} points with M={m_val}, R={r_val}")
                except Exception as e:
                    print(f"Load Error: {e}")
                    file_info_label.configure(text="❌ Load Failed", text_color="#e74c3c")

        def clamp_value(val, v_min, v_max):
            try:
                f_val = float(val)
                return max(v_min, min(f_val, v_max))
            except:
                return v_min

        def get_wave_max_freq(w_mode):
            if w_mode == "Square":
                return MAX_WAVE_FREQ_HZ_SQUARE
            return MAX_WAVE_FREQ_HZ_OTHER

        def update_frequency_limit_for_waveform(w_mode):
            if w_mode == "Custom ARB":
                # Custom ARB timing comes from the CSV dwell values, not this field.
                freq_label.configure(text="Timing: CSV dwell values")
                return
            max_freq = get_wave_max_freq(w_mode)
            freq_label.configure(text=f"Frequency ({MIN_WAVE_FREQ_HZ}-{max_freq} Hz):")
            clamped_freq = clamp_value(freq_entry.get(), MIN_WAVE_FREQ_HZ, max_freq)
            freq_entry.delete(0, "end")
            freq_entry.insert(0, f"{clamped_freq:.2f}")

        # Call this function when input boxes lose focus (FocusOut)
        def on_focus_out(event, entry, v_min, v_max):
            current_val = entry.get()
            if current_val:
                clamped = clamp_value(current_val, v_min, v_max)
                entry.delete(0, "end")
                entry.insert(0, f"{clamped:.2f}")
            update_plot()

        def update_plot(_=None):
            w_type = wave_type.get()
            ax.clear()
            ax.set_facecolor('#2b2b2b')
            ax.grid(True, linestyle=':', color='#444444')
            ax.tick_params(colors='#aaaaaa', labelsize=8)

            if w_type == "Custom ARB":
                if not self.current_arb_data:
                    ax.text(0.5, 0.5, "Please Load CSV", ha='center', va='center', color='gray')
                else:
                    v_pts = [p[0] for p in self.current_arb_data]
                    ax.step(range(len(v_pts)), v_pts, where='post', color='#f1c40f')
            else:
                # --- SAFELY GET VALUES ---
                try:
                    v1 = float(w_vmin_entry.get()) if w_vmin_entry.get() else min_v
                    v2 = float(w_vmax_entry.get()) if w_vmax_entry.get() else max_v
                    f_val = clamp_value(freq_entry.get(), MIN_WAVE_FREQ_HZ, get_wave_max_freq(w_type)) if freq_entry.get() else 100
                except ValueError:
                    v1, v2, f_val = min_v, max_v, 100

                offset = (v1 + v2) / 2
                amp = (v2 - v1) / 2

                # Time axis for visualization
                # Dynamic scaling to prevent wave squishing at high frequencies:
                t = np.linspace(0, 2 / f_val if f_val > 0 else 0.1, 500)

                if w_type == "Square":
                    duty = clamp_value(duty_entry.get(), 1, 99) / 100.0 if duty_entry.get() else 0.5
                    phase = f_val * t - np.floor(f_val * t)
                    y = np.where(phase < duty, v2, v1)
                elif w_type == "Triangle":
                    # Triangle wave formula: 2/pi * arcsin(sin(2*pi*f*t))
                    y = offset + amp * (2 / np.pi * np.arcsin(np.sin(2 * np.pi * f_val * t)))
                elif w_type == "Ramp":
                    # Ramp (Sawtooth): 2 * (f*t - floor(0.5 + f*t))
                    y = offset + amp * (2 * (f_val * t - np.floor(0.5 + f_val * t)))
                elif w_type == "Sine":
                    y = offset + amp * np.sin(2 * np.pi * f_val * t)

                ax.plot(t, y, color='#3498db', linewidth=2)
                ax.set_ylim(min_v - 0.5, max_v + 0.5)
                ax.set_title(f"Preview: {f_val} Hz", color="white", fontsize=10)

            canvas.draw()

        def set_wave_status(ch, text, color):
            if hasattr(self, 'wave_status_labels') and ch in self.wave_status_labels:
                self.wave_status_labels[ch].configure(text=text, text_color=color)
            if hasattr(self, 'wave_status_cache'):
                # Manual updates supersede the background status poll.
                self.wave_status_cache.pop(ch, None)
            self.update()

        def force_switch_off():
            self.syncing_outputs = True
            try:
                master_switch.deselect()
                master_switch.configure(text="OUTPUT OFF")
            finally:
                self.syncing_outputs = False

        def send_final_data():
            if not self.remote.is_connected: return

            w_mode = wave_type.get()
            ch = 4 if row == 0 else 5
            other = 5 if ch == 4 else 4

            # The AWG engine is channel-exclusive (one waveform at a time,
            # matching the on-device UI). Refuse to start while the other
            # channel owns a run instead of silently taking it over.
            for probe in (f"SOUR:WAVE:CH{other}:STAT?", f"SOUR:WAVE:CH{other}:ARB:STAT?"):
                status = self.remote.query(probe)
                if status and status.split(',')[0] in ("RUNNING", "STARTING"):
                    set_wave_status(ch, f"⚠ AWG busy on CH{other} — stop it first", "#e67e22")
                    force_switch_off()
                    return

            # --- CRITICAL DISTINCTION HERE ---
            if w_mode == "Custom ARB":
                # If ARB selected, route to binary transfer function
                self.send_binary_arb(ch)
            else:
                # Standard waveforms run on the firmware's built-in AWG engine —
                # the same generator the front panel uses.
                final_vmin = clamp_value(w_vmin_entry.get(), min_v, max_v)
                final_vmax = clamp_value(w_vmax_entry.get(), min_v, max_v)
                final_freq = clamp_value(freq_entry.get(), MIN_WAVE_FREQ_HZ, get_wave_max_freq(w_mode))
                final_duty = int(clamp_value(duty_entry.get(), 1, 99))

                if final_vmin > final_vmax:
                    final_vmin, final_vmax = final_vmax, final_vmin

                # Visually correct boxes
                w_vmin_entry.delete(0, "end")
                w_vmin_entry.insert(0, f"{final_vmin:.2f}")
                w_vmax_entry.delete(0, "end")
                w_vmax_entry.insert(0, f"{final_vmax:.2f}")
                freq_entry.delete(0, "end")
                freq_entry.insert(0, f"{final_freq:.2f}")
                duty_entry.delete(0, "end")
                duty_entry.insert(0, str(final_duty))

                # A previous run on this channel blocks reconfiguration.
                self.remote.stop_wave(ch)
                self.remote.send_scpi(f"SOUR:WAVE:CH{ch}:ARB:STOP")

                reply = self.remote.configure_wave(ch, w_mode, final_freq, final_duty,
                                                   final_vmin, final_vmax)
                if reply != "OK":
                    set_wave_status(ch, f"❌ Configure failed: {reply}", "#e74c3c")
                    force_switch_off()
                    return

                set_wave_status(ch, f"Starting {w_mode} @ {final_freq:.2f} Hz...", "#f39c12")
                ack = self.remote.run_wave(ch)
                if ack and ack.startswith("OK"):
                    set_wave_status(ch, f"✔ {w_mode} running @ {final_freq:.2f} Hz", "#2ecc71")
                else:
                    reason = "device faulted" if ack and "HARDWARE" in ack else (ack or "no response")
                    set_wave_status(ch, f"❌ Start failed: {reason}", "#e74c3c")
                    force_switch_off()

        def stop_waveform():
            if not self.remote.is_connected: return
            ch = 4 if row == 0 else 5
            # Stop whichever engine owns the channel; both replies are OK when idle.
            self.remote.stop_wave(ch)
            self.remote.send_scpi(f"SOUR:WAVE:CH{ch}:ARB:STOP")
            set_wave_status(ch, "⏹ Stopped", "#aaaaaa")

        def check_arb_visibility(choice):
            update_frequency_limit_for_waveform(choice)
            if choice == "Custom ARB":
                arb_frame.pack(pady=10, fill="x")
                for w in local_wave_inputs: w.configure(state="disabled", fg_color="#1a1a1a")
            else:
                arb_frame.pack_forget()
                for w in local_wave_inputs: w.configure(state="normal", fg_color="#333333")
                # Duty cycle only applies to square waves.
                if choice != "Square":
                    duty_entry.configure(state="disabled", fg_color="#1a1a1a")
            update_plot()

        def toggle_wave_panel(value):
            if value == "Waveform Gen.":
                wave_panel.pack(fill="x", pady=(0, 20), padx=20)
                slider_container.pack_forget()
                check_arb_visibility(wave_type.get())
            else:
                wave_panel.pack_forget()
                slider_container.pack(fill="x", padx=20, pady=(0, 20), before=bottom_ctrl)

        # Event Bindings
        freq_entry.bind("<KeyRelease>", lambda e: update_plot())
        w_vmin_entry.bind("<KeyRelease>", lambda e: update_plot())
        w_vmax_entry.bind("<KeyRelease>", lambda e: update_plot())

        freq_entry.bind("<FocusOut>", lambda e: update_frequency_limit_for_waveform(wave_type.get()))
        w_vmin_entry.bind("<FocusOut>", lambda e: on_focus_out(e, w_vmin_entry, min_v, max_v))
        w_vmax_entry.bind("<FocusOut>", lambda e: on_focus_out(e, w_vmax_entry, min_v, max_v))

        wave_type.configure(command=check_arb_visibility)
        mode_switch.configure(command=toggle_wave_panel)
        apply_btn.configure(command=send_final_data)
        stop_btn.configure(command=stop_waveform)
        duty_entry.bind("<KeyRelease>", lambda e: update_plot())
        def on_slider_change(v):
            v_entry.configure(text_color="#4cb5f5")
            v_entry.delete(0, "end")
            v_entry.insert(0, f"{float(v):.2f}")
            if getattr(self, 'syncing_setpoints', False):
                return
            ch_num = 4 if row == 0 else 5
            self.vset_sync_grace_until[ch_num - 1] = time.time() + 1.0
            self.remote.set_voltage(ch_num, float(v))

        slider.configure(command=on_slider_change)

        # Initial graph state
        update_plot()

        return {"type": "adj", "switch": master_switch, "i_lbl": i_lbl, "v_read_lbl": v_read_lbl,
                "ilim_lbl": ilim_lbl, "ilim_entry": curr_entry, "v_set_entry": v_entry, "slider": slider,
                "wave_type": wave_type, "freq_entry": freq_entry, "duty_entry": duty_entry,
                "vmin_entry": w_vmin_entry, "vmax_entry": w_vmax_entry,
                "wave_refresh": lambda: check_arb_visibility(wave_type.get())}

    def send_binary_arb(self, channel_num):
        try:
            if not hasattr(self, 'current_arb_data') or not self.current_arb_data:
                print("TX > ABORT: No ARB data loaded!")
                return

            v_ints = [int(float(p[0]) * 100) for p in self.current_arb_data]
            d_ints = [int(p[1]) for p in self.current_arb_data]
            total_count = len(v_ints)
            m_val, r_val = self.arb_params

            if not self.remote.is_connected:
                return

            # 1. STOP BACKGROUND POLLING (CUT TRAFFIC)
            self.pause_polling = True
            time.sleep(0.4)  # Wait briefly for current query to finish

            # The AWG engine is exclusive: uploads are refused while any
            # waveform (built-in or ARB, either channel) is active, so stop
            # everything first. Each stop replies OK even when idle.
            for ch in (4, 5):
                for cmd in (f"SOUR:WAVE:CH{ch}:STOP", f"SOUR:WAVE:CH{ch}:ARB:STOP"):
                    self.remote.ser.write(f"{cmd}\n".encode('ascii'))
                    time.sleep(0.4)
            self.remote.ser.reset_input_buffer()  # Discard old data left in buffer

            CHUNK_SIZE = 8
            print(f"\n--- SAFE UPLOAD STARTING (CH{channel_num}) ---")
            self.remote.ser.timeout = 2.0

            for i in range(0, total_count, CHUNK_SIZE):
                chunk_v = v_ints[i:i + CHUNK_SIZE]
                chunk_d = d_ints[i:i + CHUNK_SIZE]

                data_strs = [f"{v},{d}" for v, d in zip(chunk_v, chunk_d)]
                payload = f"SOUR:WAVE:CH{channel_num}:ARB:DATA {i}," + ",".join(data_strs) + "\n"

                self.remote.ser.write(payload.encode('ascii'))

                # Wait for ACK from device (skip injected data)
                ack_received = False
                response = ""
                for _ in range(5):
                    response = self.remote.ser.read_until(b"\n").decode('ascii', errors='ignore').strip()
                    if "OK:ACK" in response:
                        ack_received = True
                        break

                if not ack_received:
                    print(f"\nTX > ERROR: Device did not respond. Last read: '{response}' (Index: {i})")
                    if hasattr(self, 'arb_status_labels') and channel_num in self.arb_status_labels:
                        self.arb_status_labels[channel_num].configure(text="❌ Upload Timeout", text_color="#e74c3c")
                        self.update()
                    self.pause_polling = False  # Re-enable polling even on error
                    return

                # --- PROGRESS STATUS (UI Update) ---
                current_pts = min(i + CHUNK_SIZE, total_count)
                progress = int((current_pts / total_count) * 100)

                # Update UI label safely
                if hasattr(self, 'arb_status_labels') and channel_num in self.arb_status_labels:
                    self.arb_status_labels[channel_num].configure(
                        text=f"Uploading: {progress}% ({current_pts}/{total_count})",
                        text_color="#f39c12"
                    )
                    self.update()  # Force UI refresh

            print(f"\n\nTX > All data transmitted. Starting waveform (CH{channel_num})...")
            start_cmd = f"SOUR:WAVE:CH{channel_num}:ARB:START {total_count},{m_val},{r_val}\n"
            self.remote.ser.write(start_cmd.encode('ascii'))

            # The START ack is deferred until the output is verified running;
            # a failed start reports ERR (e.g. ERR:HARDWARE) the same way.
            final_resp = ""
            deadline = time.time() + 6.0
            while time.time() < deadline:
                line = self.remote.ser.read_until(b"\n").decode('ascii', errors='ignore').strip()
                if "OK:" in line and "STARTED" in line:
                    final_resp = line
                    break
                if "ERR" in line:
                    final_resp = line
                    break
            print(f"ARB START result: {final_resp!r}\n")

            if hasattr(self, 'arb_status_labels') and channel_num in self.arb_status_labels:
                if final_resp.startswith("OK"):
                    self.arb_status_labels[channel_num].configure(
                        text="✔ Upload Complete! Running.",
                        text_color="#2ecc71"
                    )
                else:
                    self.arb_status_labels[channel_num].configure(
                        text=f"❌ Start failed: {final_resp or 'no response'}",
                        text_color="#e74c3c"
                    )
                self.update()

        except Exception as e:
            print(f"\nTX > CRITICAL ERROR: {e}", flush=True)
            if hasattr(self, 'arb_status_labels') and channel_num in self.arb_status_labels:
                self.arb_status_labels[channel_num].configure(text="❌ Critical Error", text_color="#e74c3c")
                self.update()
        finally:
            # 2. RESTORE THE SHORT TIMEOUT AND RESTART BACKGROUND POLLING
            try:
                self.remote.ser.timeout = 0.1
            except Exception:
                pass
            self.pause_polling = False

    def load_csv_and_fill_box():
        file_path = filedialog.askopenfilename(filetypes=[("CSV files", "*.csv")])
        if not file_path: return

        with open(file_path, 'r') as f:
            reader = csv.reader(f)
            next(reader, None)  # Skip header
            lines = [f"{row[0]}:{row[1]}" for row in reader if len(row) >= 2]

        arb_entry.delete("0.0", "end")
        arb_entry.insert("0.0", ",".join(lines))


    BENCHVOLT_USB_ID = (0x0483, 0x5740)

    def _port_is_benchvolt(self, port):
        """True when the selected port enumerates with the BenchVolt USB
        identity (shared by the application, the stock bootloader, and the
        v2 boot cores). Blocks uploads aimed at debug probes etc."""
        for candidate in serial.tools.list_ports.comports():
            if candidate.device == port:
                return (candidate.vid, candidate.pid) == self.BENCHVOLT_USB_ID
        return False

    def _set_device_controls_enabled(self, enabled, parent=None):
        """Recursively enable/disable the Main Control tab's widgets."""
        if parent is None:
            parent = self.tab_main
        state = "normal" if enabled else "disabled"
        for child in parent.winfo_children():
            try:
                child.configure(state=state)
            except Exception:
                pass
            self._set_device_controls_enabled(enabled, child)

    def toggle_conn(self):
        selected_port = self.port_option_menu.get()

        if not self.remote.is_connected:
            if selected_port == "No Port Found":
                return

            if self.remote.connect(selected_port):
                # Only a device that identifies as a BenchVolt unlocks the
                # controls; anything else (debug probe, random CDC port,
                # a boot core awaiting firmware) is disconnected again.
                identity = self.remote.query('*IDN?')
                if not identity or "BenchVolt" not in identity:
                    # Distinguish "wrong device" from "BenchVolt updater":
                    # a v2 boot core answers the INFO probe with BV2C.
                    updater = False
                    try:
                        self.remote.ser.reset_input_buffer()
                        self.remote.ser.write(bytes([0x10]))
                        time.sleep(0.4)
                        updater = self.remote.ser.read(12)[:4] == b"BV2C"
                    except Exception:
                        pass
                    self.remote.disconnect()
                    if updater:
                        self.status_indicator.configure(
                            text="UPDATER MODE — use Firmware Update tab",
                            text_color="orange")
                    else:
                        self.status_indicator.configure(
                            text="NOT A BENCHVOLT", text_color="orange")
                    return
                self.status_indicator.configure(text=f"CONNECTED ({selected_port})", text_color="#2ecc71")
                self.connect_btn.configure(text="DISCONNECT", fg_color="#c0392b")
                self._set_device_controls_enabled(True)

                # 1. FIRST DO INITIAL QUERIES (Background loop sleeping)
                print(f"Device Info: {identity}")

                build_info = self.remote.get_build_date()
                if build_info:
                    clean_info = build_info.replace("  ", " ")
                    self.fw_date_info.configure(text=clean_info)

                # Auto-detect the source's PDOs and the negotiated contract,
                # and pull the on-device waveform configuration into the panel.
                self.update_pdo_dropdown()
                self.sync_wave_config_from_device()

                # 2. START BACKGROUND POLLING LOOP WHEN DONE
                self.initial_vset_sync_done = False
                self.running = True
                self.polling_thread = threading.Thread(target=self.mock_data_loop, daemon=True)
                self.polling_thread.start()

        else:
            self.running = False  # Stop loop
            self.remote.disconnect()
            self.status_indicator.configure(text="DISCONNECTED", text_color="red")
            self.connect_btn.configure(text="CONNECT (USB)", fg_color="#2ecc71")
            self.fw_date_info.configure(text="--")
            self._set_device_controls_enabled(False)

    def sync_output_switches_from_meas_all(self, parts):
        if len(parts) < 18:
            return

        try:
            output_states = [int(float(parts[i])) for i in range(13, 18)]
        except ValueError:
            return

        now = time.time()

        self.syncing_outputs = True
        try:
            for idx, state in enumerate(output_states):
                if idx >= len(self.channels):
                    continue

                if now < self.output_sync_grace_until[idx]:
                    continue

                switch = self.channels[idx].get("switch")
                if switch is None:
                    continue

                desired_state = bool(state)
                current_state = bool(switch.get())

                if current_state != desired_state:
                    if desired_state:
                        switch.select()
                    else:
                        switch.deselect()

                if idx < 3:
                    switch.configure(text="ON" if desired_state else "OFF")
                else:
                    switch.configure(text="OUTPUT ON" if desired_state else "OUTPUT OFF")
        finally:
            self.syncing_outputs = False

    def sync_limits_and_setpoints_from_meas_all(self, parts):
        if len(parts) < 27:
            return

        try:
            current_limits = [float(parts[i]) for i in range(20, 25)]
            vset_values = [float(parts[25]), float(parts[26])]
        except ValueError:
            return

        # Current limit SET entries are intentionally NOT synchronized from MEAS:ALL?.
        # They are user input fields. Only the readback labels are updated continuously.
        for idx, limit in enumerate(current_limits):
            if idx >= len(self.channels):
                continue
            ilim_lbl = self.channels[idx].get("ilim_lbl")
            if ilim_lbl is not None:
                ilim_lbl.configure(text=f"Lim: {limit:.2f} A")

        # CH4/CH5 Vset entry and slider are synchronized only once after connection.
        # Continuous synchronization makes user edits and polling fight each other.
        if self.initial_vset_sync_done:
            return

        self.syncing_setpoints = True
        try:
            for idx, vset in zip([3, 4], vset_values):
                if idx >= len(self.channels):
                    continue

                v_entry = self.channels[idx].get("v_set_entry")
                slider = self.channels[idx].get("slider")

                if slider is not None:
                    slider.set(vset)

                if v_entry is not None:
                    v_entry.delete(0, "end")
                    v_entry.insert(0, f"{vset:.2f}")
                    v_entry.configure(text_color="#4cb5f5")
        finally:
            self.syncing_setpoints = False
            self.initial_vset_sync_done = True

    def mock_data_loop(self):
        while self.running:
            # AUTO-RECONNECT: if the connection dropped (unplug, device reset),
            # keep retrying the last port until it comes back or the user
            # disconnects explicitly (which stops this loop).
            if not self.remote.is_connected:
                self.after(0, lambda: self.status_indicator.configure(
                    text="RECONNECTING...", text_color="#f39c12"))
                if self.remote.try_reconnect():
                    port = self.remote.last_port
                    self.after(0, lambda p=port: self.status_indicator.configure(
                        text=f"CONNECTED ({p})", text_color="#2ecc71"))
                    # Re-sync setpoints, the PD contract, and the waveform
                    # configuration after the drop.
                    self.initial_vset_sync_done = False
                    self.update_pdo_dropdown()
                    self.sync_wave_config_from_device()
                else:
                    time.sleep(2.0)
                    continue

            # SKIP POLLING IF PAUSE FLAG IS RAISED
            if self.remote.is_connected and not getattr(self, 'pause_polling', False):
                try:
                    raw_data = self.remote.query("MEAS:ALL?")

                    if raw_data and "," in raw_data:
                        # strip() is important to remove hidden chars (\r)
                        parts = raw_data.strip().split(",")

                        if len(parts) >= 13:
                            # 1. Update Fixed Channel Currents
                            for i in range(3):
                                self.channels[i]["i_lbl"].configure(text=f"{parts[i * 2 + 1]} A")

                            # 2. CH4: Low Range
                            self.channels[3]["v_read_lbl"].configure(text=f"{parts[6]} V")
                            self.channels[3]["i_lbl"].configure(text=f"{parts[7]} A")

                            # 3. CH5: High Range
                            self.channels[4]["v_read_lbl"].configure(text=f"{parts[8]} V")
                            self.channels[4]["i_lbl"].configure(text=f"{parts[9]} A")

                            # 4. System Info
                            self.pd_info.configure(text=f"{parts[10]}V / {parts[11]}A")
                            self.temp_info.configure(text=f"{parts[12]} °C")

                            # 5. Output State Sync (new firmware MEAS:ALL? format)
                            # Expected appended fields:
                            # OUT1,OUT2,OUT3,OUT4,OUT5,ARB4,ARB5
                            if len(parts) >= 20:
                                self.sync_output_switches_from_meas_all(parts)
                            if len(parts) >= 27:
                                self.sync_limits_and_setpoints_from_meas_all(parts)
                        else:
                            print(f"!!! Missing Packet Error: {len(parts)} parts found. Expected: 13")

                    # Poll the waveform engine every ~1 s so a fault or stop on
                    # the device is reflected in the GUI status line.
                    self._wave_poll_counter = getattr(self, '_wave_poll_counter', 0) + 1
                    if self._wave_poll_counter % 3 == 0:
                        for ch in (4, 5):
                            status = self.remote.get_wave_status(ch)
                            if status:
                                self.after(0, lambda c=ch, s=status: self._apply_wave_status(c, s))

                except Exception as e:
                    pass

            time.sleep(0.3)

    def on_close(self):
        self.running = False
        self.destroy()

    def update_pdo_dropdown(self):
        """Fetches the PDO list and negotiated contract, then updates the
        dropdown with the active PDO marked and pre-selected. Safe to call from
        the polling thread: queries run here, UI updates are scheduled."""
        if not self.remote.is_connected:
            return

        raw_list = self.remote.query_pdo_list()
        # A busy/error row inside the markers means the firmware refused the
        # read (outputs on, or the PD bus still settling); keep the previous
        # list instead of wiping the dropdown and its apply-map.
        if not raw_list or any(line.startswith("ERR") for line in raw_list):
            return
        contract = self.remote.get_pd_contract()
        self.after(0, lambda: self._apply_pdo_list(raw_list, contract))

    def _apply_wave_status(self, ch, status):
        """Reflects the device's waveform engine status in the GUI. Only
        transitions are announced, so manual status text isn't clobbered."""
        if not hasattr(self, 'wave_status_cache'):
            self.wave_status_cache = {}
        previous = self.wave_status_cache.get(ch)
        if status == previous:
            return
        self.wave_status_cache[ch] = status
        label = getattr(self, 'wave_status_labels', {}).get(ch)
        if label is None:
            return
        if status == "FAULT":
            label.configure(text="⚠ FAULT — output shut down (see device screen)",
                            text_color="#e74c3c")
        elif status == "RUNNING" and previous is not None:
            # previous=None means a manual status line was just written (e.g.
            # "Sine running @ 10 Hz"); keep it until the state actually moves.
            label.configure(text="▶ Waveform running", text_color="#2ecc71")
        elif status == "STOPPED" and previous in ("RUNNING", "STARTING", "STOPPING", "FAULT"):
            label.configure(text="⏹ Stopped", text_color="#aaaaaa")

    def sync_wave_config_from_device(self):
        """Pulls the on-device AWG configuration — the single source of truth —
        into the matching channel's waveform panel. Safe from any thread."""
        config = self.remote.get_wave_config()
        if config:
            self.after(0, lambda: self._apply_wave_config(config))

    def _apply_wave_config(self, config):
        idx = config["channel"] - 1  # CH4 -> channels[3], CH5 -> channels[4]
        if idx >= len(self.channels):
            return
        card = self.channels[idx]
        if "wave_type" not in card:
            return
        # Don't clobber a Custom ARB selection — the device query only
        # describes the built-in engine.
        if card["wave_type"].get() != "Custom ARB":
            card["wave_type"].set(config["mode"])
        for key, value in (("freq_entry", f"{config['freq_hz']:.2f}"),
                           ("duty_entry", str(config["duty"])),
                           ("vmin_entry", f"{config['low_v']:.2f}"),
                           ("vmax_entry", f"{config['high_v']:.2f}")):
            entry = card[key]
            previous_state = entry.cget("state")
            entry.configure(state="normal")
            entry.delete(0, "end")
            entry.insert(0, value)
            entry.configure(state=previous_state)
        card["wave_refresh"]()

    def _apply_pdo_list(self, raw_list, contract):
        dropdown_values = []
        self.current_pdo_data = {}  # Map data to send upon selection
        active_entry = None
        previous_selection = self.pdo_menu.get()

        for line in raw_list:
            # Incoming format: index,volt_mv,curr_ma,power_mw
            parts = line.split(',')
            if len(parts) == 4:
                idx, v_mv, i_ma, p_mw = parts
                display_str = f"PDO {idx}: {int(v_mv) / 1000}V | {int(i_ma) / 1000}A"
                if contract is not None and int(v_mv) == contract[1]:
                    display_str += " ● active"
                    active_entry = display_str
                dropdown_values.append(display_str)
                self.current_pdo_data[display_str] = (v_mv, i_ma)

        if dropdown_values:
            self.pdo_menu.configure(values=dropdown_values)
            if active_entry:
                self.pdo_menu.set(active_entry)
            else:
                # Contract unknown (e.g. older firmware): keep the user's pick
                # if it still exists, ignoring any stale active marker.
                previous = previous_selection.replace(" ● active", "")
                match = next(
                    (v for v in dropdown_values if v.replace(" ● active", "") == previous),
                    None,
                )
                self.pdo_menu.set(match or dropdown_values[0])

    def set_selected_pdo(self):
        """Writes selected PDO to slot 3 (Active)."""
        selected_text = self.pdo_menu.get()
        if hasattr(self, 'current_pdo_data') and selected_text in self.current_pdo_data:
            v_mv, i_ma = self.current_pdo_data[selected_text]
            reply = self.remote.set_pdo(3, v_mv, i_ma)
            if reply and "ERR" in reply:
                # Typically ERR:BUSY while the PD bus settles after a list
                # query; surface it instead of silently doing nothing.
                print(f"PDO apply refused ({reply.strip()}) - retry in a second")
                self.pd_info.configure(text="PD BUSY - retry", text_color="#f39c12")
            else:
                print(f"Applying PD Profile: Slot 3 | {v_mv}mV | {i_ma}mA")

    def send_manual_pdo(self):
        try:
            v_mv = int(self.man_v_entry.get())
            i_ma = int(self.man_i_entry.get())
            reply = self.remote.set_pdo(3, v_mv, i_ma)
            if reply and "ERR" in reply:
                print(f"Manual PDO apply refused ({reply.strip()}) - retry in a second")
            else:
                print(f"Applying Manual PD Profile: Slot 3 | {v_mv}mV | {i_ma}mA")
        except ValueError:
            print("Error: Manual PDO values are not valid integers!")


if __name__ == "__main__":
    app = BenchVoltUI()
    app.protocol("WM_DELETE_WINDOW", app.on_close)
    app.mainloop()
