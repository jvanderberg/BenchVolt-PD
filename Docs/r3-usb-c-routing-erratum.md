# r3 USB-C routing erratum

This note applies to the r3 schematic and boards assembled from it. The stacked
`XUBF-0336-24B02` connector does not route both CC contacts of either physical
USB-C receptacle to the STUSB4500. USB-C attachment is therefore dependent on
plug orientation, and the two receptacles are not independent power domains.

## Definitive schematic connections

The upper receptacle is labelled `A: USB PD/COMM SIDE`:

- `1A5 / CC1` is connected to the STUSB4500 `CC1` pin.
- `2B5 / CC2` is unconnected.
- `USB_A_P` and `USB_A_N` can be selected by mechanical switch S2 for MCU USB
  data.

The lower receptacle is labelled `B: USB COMM SIDE`:

- `1B5 / CC2` is connected to the STUSB4500 `CC2` pin.
- `2A5 / CC1` is unconnected.
- `USB_B_P` and `USB_B_N` can be selected by S2 for MCU USB data.

All VBUS contacts of both receptacles connect to the same `VBUS` net. There is
no power isolation between the upper and lower receptacles. Q1 separates this
shared connector-side VBUS from downstream `VBUS_SINK`; it does not isolate
the receptacles from each other. S2 switches only the USB 2.0 data pair.

## Consequences

- The upper PD/COMM receptacle can advertise the STUSB4500 sink termination in
  only one plug orientation. In the other orientation the source sees the
  unconnected `2B5 / CC2` contact, so the controller remains detached and PDO
  negotiation cannot start.
- Flipping the USB-C plug at the board changes whether the routed CC contact is
  used. A working orientation should change `SYST:PD:RAW?` from `PORT0x00
  CC0x20 TYPEC0x01` to an attached state before any PD command is attempted.
- Connecting a normal powered USB-A-to-USB-C COM cable to the lower receptacle
  while a PD source powers the upper receptacle connects both sources to the
  same VBUS net. This is not a supported or safe operating arrangement.
- Legacy C firmware could appear to work when the upper cable happened to use
  its routed CC orientation. It also allowed outputs without verifying a PD
  contract, so powered outputs alone did not prove successful negotiation.

## Safe connection modes

### One cable for power and data

Use only the upper `USB PD/COMM` receptacle. The other receptacle must remain
unconnected.

1. Connect the upper receptacle to a USB-C host or dock that supplies both USB
   data and PD power. A charger-only PPS brick does not provide the USB host
   data connection required for CDC.
2. Put mechanical switch S2 in the `USB_A_P` / `USB_A_N` position so the MCU
   data pair is connected to the upper receptacle.
3. If the STUSB4500 reports detached, flip the USB-C plug at the board because
   only one CC orientation is routed.
4. Confirm CDC enumeration, then query `SYST:PD:RAW?`. Require an attached
   Type-C state and a nonzero RDO before enabling an output.

The available fixed PDOs and maximum power are determined by the USB-C host or
dock. A host that offers data but only 5 V cannot provide the brick's higher
power merely because the cable supports it.

### Separate PPS source and COM connection

Do not use an ordinary powered USB-A COM cable in this mode. The COM path must
have its VBUS conductor absent or blocked while retaining D+, D-, and ground,
for example with a purpose-built USB VBUS blocker/data-only adapter. Without
such isolation, disconnect the USB-A COM cable before connecting the PPS
source.

## Required board correction

A corrected revision must route both CC contacts of the intended PD/COMM
receptacle to the corresponding STUSB4500 `CC1` and `CC2` pins. The COM
receptacle must not place a second external source directly onto the PD VBUS
net; add appropriate power-path isolation or omit its VBUS connection according
to the intended USB architecture.

Source: [`Schematics/USB_PowerSupply_r3.pdf`](../Schematics/USB_PowerSupply_r3.pdf).
