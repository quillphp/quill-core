#ifndef QUILL_H
#define QUILL_H

void*  quill_router_build(const char* manifest_json, size_t manifest_len);
int    quill_router_match(void* router, const char* method, size_t method_len,
                           const char* path, size_t path_len,
                           uint32_t* out_handler_id,
                           uint32_t* out_num_params,
                           char* out_params_json, size_t out_params_max);
void   quill_router_free(void* router);
size_t quill_json_compact(const char* input, size_t input_len,
                           char* out_buf, size_t out_max);
int    quill_router_dispatch(void* router, void* validator, 
                             const char* method, size_t method_len,
                             const char* path, size_t path_len,
                             const char* body_json, size_t body_len,
                             char* out_json, size_t out_max);
void*  quill_validator_new();
int    quill_validator_register(void* registry, const char* name, size_t name_len, const char* schema_json, size_t schema_len);
int    quill_validator_validate(void* registry, const char* dto_name, size_t dto_name_len, const char* input_json, size_t input_len, char* out_json, size_t out_max);
void   quill_validator_free(void* registry);

int quill_server_listen(void* router, void* validator, uint16_t port);
int quill_server_prebind(uint16_t port);
int quill_server_poll(void* out_id, void* out_handler_id, char* out_params_json, size_t out_params_max, char* out_dto_json, size_t out_dto_max);
int quill_server_respond(uint32_t id, const char* response_json, size_t response_len);

#endif
