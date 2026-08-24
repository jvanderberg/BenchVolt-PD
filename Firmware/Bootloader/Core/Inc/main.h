/* USER CODE BEGIN Header */
/**
  ******************************************************************************
  * @file           : main.h
  * @brief          : Header for main.c file.
  * This file contains the common defines of the application.
  ******************************************************************************
  * @attention
  *
  * Copyright (c) 2026 STMicroelectronics.
  * All rights reserved.
  *
  * This software is licensed under terms that can be found in the LICENSE file
  * in the root directory of this software component.
  * If no LICENSE file comes with this software, it is provided AS-IS.
  *
  ******************************************************************************
  */
/* USER CODE END Header */

/* Define to prevent recursive inclusion -------------------------------------*/
#ifndef __MAIN_H
#define __MAIN_H

#ifdef __cplusplus
extern "C" {
#endif

/* Includes ------------------------------------------------------------------*/
#include "stm32f0xx_hal.h"

/* Private includes ----------------------------------------------------------*/
/* USER CODE BEGIN Includes */

// Memory Map Definitions based on 128KB total Flash
#define BOOTLOADER_SIZE_BYTES       (32U * 1024U)
#define MAIN_APP_FLASH_ADDR         0x08008000U
#define MAIN_APP_SIZE_MAX_BYTES     (92U * 1024U)
#define MAIN_APP_END_ADDR           (MAIN_APP_FLASH_ADDR + MAIN_APP_SIZE_MAX_BYTES)
#define SETTINGS_PAGE_ADDR          0x0801F000U
#define PARAM_PAGE_ADDR             0x0801F800U
#define FLASH_END_ADDR              0x08020000U

#if MAIN_APP_END_ADDR != SETTINGS_PAGE_ADDR
#error "Application partition must end at the settings page"
#endif
#if (PARAM_PAGE_ADDR + 0x800U) != FLASH_END_ADDR
#error "Boot metadata must occupy the final flash page"
#endif



/* USER CODE END Includes */

/* Exported types ------------------------------------------------------------*/
/* USER CODE BEGIN ET */
void WriteToDevice(char* commandString);
/* USER CODE END ET */

/* Exported constants --------------------------------------------------------*/
/* USER CODE BEGIN EC */

/* USER CODE END EC */

/* Exported macro ------------------------------------------------------------*/
/* USER CODE BEGIN EM */

/* USER CODE END EM */

/* Exported functions prototypes ---------------------------------------------*/
void Error_Handler(void);

/* USER CODE BEGIN EFP */

/* USER CODE END EFP */

/* Private defines -----------------------------------------------------------*/
#define TFT_DC_Pin GPIO_PIN_10
#define TFT_DC_GPIO_Port GPIOC
#define TFT_RES_Pin GPIO_PIN_11
#define TFT_RES_GPIO_Port GPIOC
#define SPI1_CS_Pin GPIO_PIN_2
#define SPI1_CS_GPIO_Port GPIOD
#define LED_RED_Pin GPIO_PIN_8
#define LED_RED_GPIO_Port GPIOB
#define LED_BLUE_Pin GPIO_PIN_9
#define LED_BLUE_GPIO_Port GPIOB

/* USER CODE BEGIN Private defines */
void Bootloader_ShowProgress(uint32_t writtenBytes, uint32_t totalBytes);
/* USER CODE END Private defines */

#ifdef __cplusplus
}
#endif

#endif /* __MAIN_H */
