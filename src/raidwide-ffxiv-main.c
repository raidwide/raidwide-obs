/*
Raidwide
Copyright (C) 2026 Julien CLEMENT

This program is free software; you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation; either version 2 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License along
with this program. If not, see <https://www.gnu.org/licenses/>
*/

#include <obs-module.h>
#include <obs-frontend-api.h>
#include <plugin-support.h>

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE(PLUGIN_NAME, "en-US")

/*
struct raidwide_data {
	obs_encoder_t *inner_encoder;
	obs_encoder_t *encoder_self;
};

const char *get_name(struct raidwide_data *type_data)
{
	UNUSED_PARAMETER(type_data);
	return "Raidwide FFXIV (H.264)";
}

void *create(obs_data_t *settings, obs_encoder_t *encoder)
{
	struct raidwide_data *data = bzalloc(sizeof(struct raidwide_data));
	data->inner_encoder = obs_video_encoder_create("obs_x264", "raidwide_inner_x264", settings, NULL);
	data->encoder_self = encoder;
	return data;
}

void destroy(struct raidwide_data *data)
{
	obs_encoder_release(data->encoder_self);
	bfree(data);
}

bool encode(struct raidwide_data *data, struct encoder_frame *frame,
                            struct encoder_packet *packet,
                            bool *received_packet)
{
	UNUSED_PARAMETER(data);
	UNUSED_PARAMETER(frame);
	UNUSED_PARAMETER(packet);
	UNUSED_PARAMETER(received_packet);
	return false;
}

struct obs_encoder_info raidwide_h264_encoder = {
	.id = "raidwide_ffxiv_h264",
	.type = OBS_ENCODER_VIDEO,
	.codec = "h264",
	.get_name = get_name,
	.create = create,
	.destroy = destroy,
	.encode = encode,
};
*/

#define SEI_UUID_SIZE 16
#define SEI_START_CODE_SIZE 4
#define SEI_NAL_TRAILING_SIZE 1


struct nal_info
{
	size_t payload_size;
	size_t total_nal_size;
	size_t sei_size;
	size_t sei_size_n_bytes;
};

struct nal_info get_nal_info(size_t payload_size)
{
	struct nal_info res;
	res.sei_size = payload_size + SEI_UUID_SIZE;
	res.sei_size_n_bytes = 1 + (res.sei_size / 255);
	res.total_nal_size = SEI_START_CODE_SIZE + 1 + 1 + res.sei_size_n_bytes + res.sei_size  + SEI_NAL_TRAILING_SIZE;
	res.payload_size = payload_size;
	return res;
}

void emplace_sei_nal(uint8_t *dst, uint8_t *payload, const uint8_t *uuid, struct nal_info nal_info)
{
	// Start code
	*dst++ = 0;
	*dst++ = 0;
	*dst++ = 0;
	*dst++ = 1;

	// NAL header (SEI = 6)
	*dst++ = 6;
	// Payload type (Unregistered User Data = 5)
	*dst++ = 5;

	size_t s = nal_info.sei_size;
	while (s >= 0xff)
	{
		*dst++ = 0xff;
		s -= 0xff;
	}
	*dst++ = (uint8_t) s;

	memcpy(dst, uuid, SEI_UUID_SIZE);
	dst += SEI_UUID_SIZE;

	memcpy(dst, payload, nal_info.payload_size);
	dst += nal_info.payload_size;

	// Trailing byte
	*dst++ = 0x80;
}

void packet_cb(obs_output_t *output, struct encoder_packet *pkt, struct encoder_packet_time *pkt_time, void *data)
{
	obs_log(LOG_INFO, "Callback on output: %s", obs_output_get_name(output));
	obs_log(LOG_INFO, "Callback on output: %s", obs_output_get_id(output));
	UNUSED_PARAMETER(data);
	UNUSED_PARAMETER(pkt_time);
	if (pkt->type != OBS_ENCODER_VIDEO)
	{
		return;
	}
	obs_log(LOG_INFO, "Received packet of size %d on output %s", pkt->size, obs_output_get_name(output));

	char *payload = "SEI RAIDWIDE";
	size_t payload_size = strlen(payload);
	struct nal_info nal_info = get_nal_info(payload_size);

	size_t old_size = pkt->size;
	pkt->size += nal_info.total_nal_size;
	pkt->data = brealloc(pkt->data, pkt->size);
	memmove(pkt->data + nal_info.total_nal_size, pkt->data, old_size);

	const uint8_t uuid[16] = {0x72, 0x61, 0x69, 0x64, 0x77, 0x69, 0x64, 0x65,
						0xff, 0x14, 0xff, 0x14, 0xff, 0x14, 0xff, 0x14};

	emplace_sei_nal(pkt->data, (uint8_t *) payload, uuid, nal_info);
}

bool enum_output_cb(void *data, obs_output_t *output)
{
	UNUSED_PARAMETER(data);
	obs_log(LOG_INFO, "Found output: %s", obs_output_get_name(output));
	obs_output_remove_packet_callback(output, packet_cb, NULL);
	obs_output_add_packet_callback(output, packet_cb, NULL);
	return true;
}

void frontend_event_cb(enum obs_frontend_event event, void *data)
{
	UNUSED_PARAMETER(data);
	if (event == OBS_FRONTEND_EVENT_STREAMING_STARTED || event == OBS_FRONTEND_EVENT_RECORDING_STARTED)
	{
		obs_log(LOG_INFO, "Recording or streaming started, hooking output for raidwide data");
		obs_enum_outputs(enum_output_cb, NULL);
	}
}


bool obs_module_load(void)
{
	//obs_log(LOG_INFO, "Registering raidwide h264 encoder");
	//obs_register_encoder(&raidwide_h264_encoder);

	obs_frontend_add_event_callback(frontend_event_cb, NULL);
	
	obs_log(LOG_INFO, "Raidwide FFXIV plugin loaded successfully (version %s)", PLUGIN_VERSION);

	return true;
}

void obs_module_unload(void)
{
	obs_log(LOG_INFO, "plugin unloaded");
}
