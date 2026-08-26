import serial
import time
import threading

class BenchVoltRemote:
    """Handles SCPI communication with the BenchVolt-PD hardware."""

    def __init__(self):
        self.ser = None
        self.is_connected = False
        self.last_port = None
        # Use an RLock to prevent deadlocks during nested calls
        self.lock = threading.RLock()

    def connect(self, port, baudrate=115200):
        """Connects to the STM32 via USB CDC."""
        try:
            # Use a low timeout to prevent UI freezing during serial operations
            self.ser = serial.Serial(port, baudrate, timeout=0.1, exclusive=True)
            self.is_connected = True
            self.last_port = port
            return True
        except Exception as e:
            print(f"Connection Error: {e}")
            return False

    def try_reconnect(self):
        """Attempts to reopen the last known port after a dropped connection."""
        if self.is_connected or not self.last_port:
            return False
        with self.lock:
            try:
                if self.ser and self.ser.is_open:
                    self.ser.close()
            except Exception:
                pass
            try:
                self.ser = serial.Serial(self.last_port, 115200, timeout=0.1, exclusive=True)
                self.is_connected = True
            except Exception:
                return False
        # Prove the device is actually responsive, not just enumerated.
        if self.query("*IDN?"):
            print(f"Reconnected to {self.last_port}")
            return True
        self.disconnect()
        return False

    def disconnect(self):
        """Closes the serial connection safely."""
        with self.lock:  # Prevent closing the port while another thread is reading
            if self.ser and self.ser.is_open:
                try:
                    self.ser.close()
                except Exception:
                    pass  # Ignore the error if the USB connection was already physically lost
            self.is_connected = False

    def send_scpi(self, command):
        """Sends a raw SCPI command to the device."""
        if self.is_connected:
            with self.lock:  # Serialize access to the serial port
                try:
                    print(f"TX > {command}")
                    self.ser.write(f"{command}\n".encode('ascii'))
                except Exception as e:
                    print(f"Write Error: Device connection was lost! ({e})")
                    self.is_connected = False  # Treat the device as disconnected after a communication failure

    def query(self, command):
        """Sends a query and waits for a response safely."""
        if self.is_connected:
            with self.lock:  # Serialize access to the serial port
                try:
                    self.ser.reset_input_buffer()
                    self.ser.write(f"{command}\n".encode('ascii'))
                    time.sleep(0.1)

                    if self.ser.in_waiting > 0:
                        raw = self.ser.readline().decode('ascii').strip()
                        return raw
                    return None
                except Exception as e:
                    print(f"Remote Query Error: USB connection was physically lost! ({e})")
                    self.is_connected = False  # Treat the device as disconnected after a communication failure
                    return None
        return None

    # --- High-Level Control API ---
    def set_voltage(self, channel, voltage):
        """Sets the target voltage for CH4 or CH5."""
        self.send_scpi(f"SOUR:VOLT:CH{channel} {voltage:.2f}")

    def set_pdo(self, slot, v_mv, i_ma):
        """Sets a specific PDO slot with target voltage (mV) and current (mA)."""
        self.send_scpi(f"SOUR:PDO:SET {slot} {v_mv} {i_ma}")

    def get_build_date(self):
        """Queries the firmware build date and time."""
        return self.query("SYST:BUILD?")

    def set_output(self, channel, state):
        """Enables (1) or Disables (0) a specific output channel."""
        self.send_scpi(f"OUTP:CH{channel}:STAT {state}")

    def get_voltage_measurement(self, channel):
        """Reads the actual voltage measurement from the device."""
        response = self.query(f"MEAS:VOLT:CH{channel}?")
        try:
            return float(response)
        except (ValueError, TypeError):
            return 0.0

    # --- Built-in waveform engine (SOUR:WAVE:CHn:...) ---
    WAVE_FUNC_CODES = {"Square": "SQU", "Triangle": "TRI", "Ramp": "RAMP", "Sine": "SIN"}

    def configure_wave(self, channel, wave_mode, freq_hz, duty_percent, low_v, high_v):
        """Configures the on-device waveform engine. Returns the OK/ERR reply."""
        func = self.WAVE_FUNC_CODES[wave_mode]
        freq_millihz = int(round(float(freq_hz) * 1000))
        low_mv = int(round(float(low_v) * 1000))
        high_mv = int(round(float(high_v) * 1000))
        return self.query(
            f"SOUR:WAVE:CH{channel}:FUNC {func},{freq_millihz},{int(duty_percent)},{low_mv},{high_mv}"
        )

    def run_wave(self, channel, timeout=5.0):
        """Starts the configured waveform. The firmware acknowledges only after
        the output is actually running, so wait beyond the usual query window."""
        if not self.is_connected:
            return None
        with self.lock:
            try:
                self.ser.reset_input_buffer()
                command = f"SOUR:WAVE:CH{channel}:RUN"
                print(f"TX > {command}")
                self.ser.write(f"{command}\n".encode('ascii'))
                deadline = time.time() + timeout
                while time.time() < deadline:
                    line = self.ser.readline().decode('ascii', errors='ignore').strip()
                    if "OK" in line or "ERR" in line:
                        return line
            except Exception as e:
                print(f"Wave Start Error: Device communication was lost! ({e})")
                self.is_connected = False
                return None
        # No ack within the window — ask the engine directly so a late fault
        # (or a late success) is still reported instead of silently lost.
        status = self.get_wave_status(channel)
        if status:
            if status.startswith("RUNNING"):
                return "OK:LATE"
            if status.startswith("FAULT"):
                return "ERR:HARDWARE"
        return None

    def stop_wave(self, channel):
        """Stops a running built-in waveform. Returns the OK/ERR reply."""
        return self.query(f"SOUR:WAVE:CH{channel}:STOP")

    def get_wave_status(self, channel):
        """Returns RUNNING/STARTING/STOPPING/FAULT/STOPPED for the engine."""
        return self.query(f"SOUR:WAVE:CH{channel}:STAT?")

    def get_wave_config(self):
        """Reads the on-device AWG configuration (the single source of truth
        for the waveform panel). Returns a dict or None."""
        response = self.query("SOUR:WAVE:FUNC?")
        if not response or not response.startswith("CH"):
            return None
        try:
            fields = response.split(',')
            names = {"SQU": "Square", "TRI": "Triangle", "RAMP": "Ramp", "SIN": "Sine"}
            return {
                "channel": int(fields[0][2:]),
                "mode": names[fields[1]],
                "freq_hz": int(fields[2]) / 1000.0,
                "duty": int(fields[3]),
                "low_v": int(fields[4]) / 1000.0,
                "high_v": int(fields[5]) / 1000.0,
            }
        except (ValueError, KeyError, IndexError):
            return None

    def get_pd_contract(self):
        """Returns the negotiated PD contract as (position, mv, ma), or None."""
        response = self.query("SYST:PD:CONTRACT?")
        if not response or response == "NONE":
            return None
        try:
            position, mv, ma = (int(field) for field in response.split(','))
            return position, mv, ma
        except ValueError:
            return None

    def query_pdo_list(self):
        """Sends the SOUR:PD:LIST? query and collects data between the response markers."""
        if not self.is_connected:
            return []

        with self.lock:  # Serialize access to the serial port here as well
            try:
                # Clear stale data before sending the query
                self.ser.reset_input_buffer()
                print("TX > SOUR:PD:LIST?")
                self.ser.write(b"SOUR:PD:LIST?\n")

                pdo_lines = []
                recording = False
                start_time = time.time()

                # Listen to the port for up to 2 seconds or until the END marker is received
                while (time.time() - start_time) < 2.0:
                    if self.ser.in_waiting > 0:
                        line = self.ser.readline().decode('ascii').strip()

                        if "UI_PDO_LIST_START" in line:
                            recording = True
                            continue

                        if "UI_PDO_LIST_END" in line:
                            recording = False
                            break  # Data transfer completed

                        if recording and line:
                            pdo_lines.append(line)

                return pdo_lines
            except Exception as e:
                print(f"PDO Query Error: Device communication was lost! ({e})")
                self.is_connected = False
                return []