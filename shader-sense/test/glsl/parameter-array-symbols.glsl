#version 450

struct Light {
    vec3 color;
};

Light getLight(Light inputLight) {
    Light lights[2];
    Light copyLight = inputLight;
    return lights[0];
}

void main() {
    Light outsideLight;
}