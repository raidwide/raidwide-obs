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

void packet_cb(obs_output_t *output, struct encoder_packet *pkt, struct encoder_packet_time *pkt_time, void *data)
{
	UNUSED_PARAMETER(data);
	UNUSED_PARAMETER(pkt);
	UNUSED_PARAMETER(pkt_time);
	obs_log(LOG_INFO, "Received packet on output %s", obs_output_get_name(output));
}

bool enum_output_cb(void *data, obs_output_t *output)
{
	UNUSED_PARAMETER(data);
	obs_log(LOG_INFO, "Found output: %s", obs_output_get_name(output));
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
