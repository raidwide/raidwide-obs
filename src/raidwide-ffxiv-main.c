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
#include <plugin-support.h>

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE(PLUGIN_NAME, "en-US")

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

bool obs_module_load(void)
{
	//obs_log(LOG_INFO, "Registering raidwide h264 encoder");
	//obs_register_encoder(&raidwide_h264_encoder);

	obs_log(LOG_INFO, "Raidwide FFXIV plugin loaded successfully (version %s)", PLUGIN_VERSION);

	return true;
}

void obs_module_unload(void)
{
	obs_log(LOG_INFO, "plugin unloaded");
}
