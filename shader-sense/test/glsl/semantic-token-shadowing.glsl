#version 450

float testValue(float value) {
    float sum = value;
    {
        float value = 1.0;
        sum += value;
    }
    return value + sum;
}

void main() {
    float result = testValue(1.0);
}