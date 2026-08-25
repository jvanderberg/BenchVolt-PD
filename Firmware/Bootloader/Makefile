# Command-line build for the BenchVolt bootloader (STM32F070RB, Cortex-M0).
# Mirrors the STM32CubeIDE project settings (.cproject): -Os, HAL defines,
# nano.specs, gc-sections, linked at 0x08000000 (32K) via STM32F070RBTX_FLASH.ld.
TARGET  := bootloader
BUILD   := build
PREFIX  ?= arm-none-eabi-
CC      := $(PREFIX)gcc
OBJCOPY := $(PREFIX)objcopy
SIZE    := $(PREFIX)size

MCU     := -mcpu=cortex-m0 -mthumb
DEFS    := -DUSE_HAL_DRIVER -DSTM32F070xB
INCLUDES := \
  -ICore/Inc \
  -IUSB_DEVICE/App \
  -IUSB_DEVICE/Target \
  -IDrivers/STM32F0xx_HAL_Driver/Inc \
  -IDrivers/STM32F0xx_HAL_Driver/Inc/Legacy \
  -IMiddlewares/ST/STM32_USB_Device_Library/Core/Inc \
  -IMiddlewares/ST/STM32_USB_Device_Library/Class/CDC/Inc \
  -IDrivers/CMSIS/Device/ST/STM32F0xx/Include \
  -IDrivers/CMSIS/Include

CFLAGS  := $(MCU) $(DEFS) $(INCLUDES) -Os -g3 -Wall \
           -ffunction-sections -fdata-sections -fstack-usage --specs=nano.specs
ASFLAGS := $(MCU) -g3 --specs=nano.specs
LDSCRIPT := STM32F070RBTX_FLASH.ld
LDFLAGS := $(MCU) --specs=nano.specs -T$(LDSCRIPT) -Wl,--gc-sections \
           -Wl,-Map=$(BUILD)/$(TARGET).map -static \
           -Wl,--start-group -lc -lm -Wl,--end-group

# All C sources (no *_template.c present; .cproject excludes nothing).
C_SOURCES := $(wildcard Core/Src/*.c) \
             $(wildcard USB_DEVICE/App/*.c) \
             $(wildcard USB_DEVICE/Target/*.c) \
             $(wildcard Drivers/STM32F0xx_HAL_Driver/Src/*.c) \
             $(wildcard Middlewares/ST/STM32_USB_Device_Library/Core/Src/*.c) \
             $(wildcard Middlewares/ST/STM32_USB_Device_Library/Class/CDC/Src/*.c)
C_SOURCES := $(filter-out %_template.c,$(C_SOURCES))
S_SOURCES := Core/Startup/startup_stm32f070rbtx.s

OBJS := $(addprefix $(BUILD)/,$(C_SOURCES:.c=.o) $(S_SOURCES:.s=.o))

all: $(BUILD)/$(TARGET).elf $(BUILD)/$(TARGET).bin

$(BUILD)/%.o: %.c
	@mkdir -p $(dir $@)
	$(CC) -c $(CFLAGS) -o $@ $<

$(BUILD)/%.o: %.s
	@mkdir -p $(dir $@)
	$(CC) -c $(ASFLAGS) -o $@ $<

$(BUILD)/$(TARGET).elf: $(OBJS) $(LDSCRIPT)
	$(CC) $(OBJS) $(LDFLAGS) -o $@
	$(SIZE) $@

$(BUILD)/$(TARGET).bin: $(BUILD)/$(TARGET).elf
	$(OBJCOPY) -O binary $< $@

clean:
	rm -rf $(BUILD)

.PHONY: all clean
