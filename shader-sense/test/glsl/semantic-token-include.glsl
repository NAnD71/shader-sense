#version 450

#extension GL_GOOGLE_include_directive : require
#include "./inc_semantic/light.glsl"

Light getLight(Light inputLight) {
    return inputLight;
}

void main() {
    Light light;
    light.color = getLight(light).color;
}