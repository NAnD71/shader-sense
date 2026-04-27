#version 450

struct Light {
    vec3 color;
};

Light getLight(Light inputLight) {
    Light copyLight = inputLight;
    copyLight.color = inputLight.color;
    return copyLight;
}

void main() {
    Light lights[2];
    lights[0].color = getLight(lights[0]).color;
}