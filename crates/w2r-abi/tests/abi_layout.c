/* Compiled as C11, C++17 and (if available) 32-bit by tools/check_abi.sh. Never run. */
#include <stddef.h>
#include "w2r_abi.h"

#ifdef __cplusplus
#define CHECK(c) static_assert(c, #c)
#else
#define CHECK(c) _Static_assert(c, #c)
#endif

CHECK(W2R_ABI_VERSION == 1);
CHECK(offsetof(W2rHost, struct_size) == 0);
CHECK(offsetof(W2rHost, abi_version) == 4);
CHECK(offsetof(W2rHost, token) == 8);
CHECK(offsetof(W2rPlugin, struct_size) == 0);
CHECK(offsetof(W2rUi, struct_size) == 0);
CHECK(W2R_UI_COMBO_SIZE == sizeof(W2rUi));
CHECK(W2R_UI_COMBO_SIZE > W2R_UI_COLOR_EDIT_SIZE);
CHECK(W2R_UI_COLOR_EDIT_SIZE > W2R_UI_TEXT_EDIT_SIZE);
CHECK(W2R_UI_TEXT_EDIT_SIZE == (sizeof(void *) == 4 ? 32 : 64));
CHECK(offsetof(W2rEvent, struct_size) == 0);
CHECK(offsetof(W2rMapDraw, struct_size) == 0);
CHECK(offsetof(W2rMapDraw, fill_rect) == (sizeof(void *) == 4 ? 32 : 40));
CHECK(W2R_MAP_DRAW_V1_SIZE == (sizeof(void *) == 4 ? 40 : 56));
CHECK(offsetof(W2rStr, len) == sizeof(void *));
CHECK(sizeof(W2rStatus) == 4);
CHECK(W2R_OK == 0);
CHECK(W2R_E_DENIED < 0 && W2R_E_BUSY < 0);
CHECK(W2R_HOST_CREATE_UI_SIZE > offsetof(W2rHost, symbol_address));
CHECK(W2R_HOST_MAP_FOLDER_SIZE > offsetof(W2rHost, create_map_select));
CHECK(W2R_HOST_SETTINGS_DECLARE_SIZE == sizeof(W2rHost));
CHECK(W2R_HOST_SETTINGS_DECLARE_SIZE > W2R_HOST_MAP_FOLDER_SIZE);
CHECK(offsetof(W2rSettingDef, struct_size) == 0);
CHECK(offsetof(W2rSettingDef, default_int) == 8);
CHECK(offsetof(W2rSettingDef, flags) == 32);
CHECK(offsetof(W2rSettingDef, choices) == 40);
CHECK(offsetof(W2rSettingDef, key) == (sizeof(void *) == 4 ? 48 : 56));
CHECK(offsetof(W2rSettingDef, active_when) == (sizeof(void *) == 4 ? 96 : 152));
CHECK(sizeof(W2rSettingDef) == (sizeof(void *) == 4 ? 104 : 168));
CHECK(W2R_SETTING_DEF_V1_SIZE == sizeof(W2rSettingDef));
CHECK(W2R_MAX_PEER_PAYLOAD + W2R_PEER_HEADER_BYTES <= 989);

/* The smallest possible plugin must compile against the header. */
static W2rStatus W2R_CALL on_load(const W2rHost *host) {
    W2rStr msg = {"hello", 5};
    return host->log(host->token, W2R_LOG_INFO, msg);
}
static const W2rPlugin kPlugin = {sizeof(W2rPlugin), on_load, 0, 0, 0};

W2R_EXPORT const W2rPlugin *W2R_CALL w2r_plugin_main(const W2rHost *host) {
    return host->abi_version >= 1 ? &kPlugin : 0;
}
