#version 450

struct Light {
    vec3 color;
};

const uint MAX_LIGHTS = 2u;

Light getLight(Light inputLight) {
    Light lights[2];
    Light configurableLights[MAX_LIGHTS];
    Light copyLight = inputLight;
    return lights[0];
}

void main() {
    Light outsideLight;
}