#version 330 core

uniform ivec2 flipAxis;
uniform int resolution;
uniform vec3 position;
uniform vec3 scale;
uniform mat4 modelViewProjection;

uniform sampler2D heights;
uniform sampler2D slopes;
uniform sampler2D noise;

uniform float noiseWavelength;
uniform vec2 textureOffset;
uniform float textureStep;

in uvec2 vPosition;
out vec2 rawTexCoord;
out vec2 texCoord;
out vec3 fPosition;

/// Uses a fractal to refine the height and slope sourced from the course texture.
void compute_height_and_slope(inout float height, inout vec2 slope) {
	float scale = 10.0;
	float wavelength = 32.0;
	for(int i = 0; i < 6; i++) {
		float smoothing = mix(0.01, 0.15, smoothstep(0.25, 0.35, length(slope)));
		vec3 v = texture(noise, fPosition.xz * noiseWavelength / wavelength).rgb;
		height += v.x * scale * smoothing;
		slope += v.yz * scale * smoothing / wavelength;

		scale *= 0.5;
		wavelength *= 0.5;
	}
}
void main() {
  vec2 iPosition = mix(ivec2(vPosition), ivec2(resolution-1) - ivec2(vPosition), flipAxis);

  vec2 tPosition = textureOffset + iPosition * textureStep;
  rawTexCoord = textureOffset + iPosition * textureStep;
  texCoord = (vec2(tPosition) + vec2(0.5)) / textureSize(heights, 0);

  vec2 p = iPosition / vec2(resolution - 1);
  fPosition = vec3(p.x, 0, p.y) * scale + position;

  float y = texture(heights, texCoord).r;
  vec2 slope = texture(slopes, texCoord).xy;
  compute_height_and_slope(y, slope);

  fPosition = vec3(p.x, y, p.y) * scale + position;
  gl_Position = modelViewProjection * vec4(fPosition, 1.0);
}
