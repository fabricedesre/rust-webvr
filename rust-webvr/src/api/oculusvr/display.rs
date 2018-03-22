#![cfg(target_os="android")]
#![cfg(feature = "oculusvr")]

use {VRDisplay, VRDisplayData, VRDisplayCapabilities, VREvent, VRDisplayEvent, 
    VREyeParameters, VRFramebuffer, VRFramebufferAttributes, VRFrameData, VRLayer, VRViewport};
use android_injected_glue::ffi as ndk;
use gl;
use egl;
use ovr_mobile_sys as ovr;
use ovr_mobile_sys::ovrFrameLayerEye::*;
use ovr_mobile_sys::ovrSystemProperty::*;
use std::cell::{Cell, RefCell};
use std::ffi::CStr;
use std::mem;
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::str;
use std::sync::{Arc, Condvar, Mutex};
use super::gamepad::{OculusVRGamepad, OculusVRGamepadPtr};
use rust_webvr_api::jni_utils::JNIScope;
use super::service::{OVRJava, OVRServiceJava};
use rust_webvr_api::utils;

pub type OculusVRDisplayPtr = Arc<RefCell<OculusVRDisplay>>;
const OVR_INSERT_FENCE: bool = false;

extern {
    fn ANativeWindow_fromSurface(env: *mut c_void, surface: *mut c_void) -> *mut c_void;
}

#[derive(Clone, Copy)]
enum LifeCycleAction {
    Resume,
    Pause,
}

pub struct OculusVRDisplay {
    display_id: u32,
    ovr: *mut ovr::ovrMobile,
    service_java: OVRServiceJava,
    // Used in the data query thread. Shared with OVRService.
    data_ovr_java: *const ovr::ovrJava,
    // Used in the render thread
    render_ovr_java: OVRJava,
    eye_framebuffers: Vec<OculusEyeFramebuffer>,
    read_fbo: u32,
    read_texture: u32,
    frame_index: i64,
    predicted_display_time: f64,
    predicted_tracking: ovr::ovrTracking2,
    eye_projection: Cell<ovr::ovrMatrix4f>,
    presenting: bool,
    activity_paused: bool,
    new_events_hint: bool,
    events: Mutex<Vec<VREvent>>,
    new_pending_action_hint: bool,
    pending_action: Mutex<Option<LifeCycleAction>>,
    // waiting for an event to occur. 
    leave_vr_condition: (Mutex<bool>, Condvar),
    // Gamepads linked to this display
    gamepads: Vec<OculusVRGamepadPtr>,
    attributes: VRFramebufferAttributes,
}

unsafe impl Send for OculusVRDisplay {}
unsafe impl Sync for OculusVRDisplay {}

impl VRDisplay for OculusVRDisplay {

    fn id(&self) -> u32 {
        self.display_id
    }

    fn data(&self) -> VRDisplayData {
        let mut data = VRDisplayData::default();

        data.display_name = "Oculus VR".into();
        data.display_id = self.display_id;
        data.connected = true;
    
        self.fetch_capabilities(&mut data.capabilities);
        self.fetch_eye_parameters(&mut data.left_eye_parameters, &mut data.right_eye_parameters);
        
        data.stage_parameters = None;

        data
    }

    fn inmediate_frame_data(&self, near: f64, far: f64) -> VRFrameData {
        let mut data = VRFrameData::default();

        if !self.activity_paused && self.is_in_vr_mode() {
            let tracking = unsafe { ovr::vrapi_GetPredictedTracking2(self.ovr, 0.0) };
            self.fetch_frame_data(self.data_ovr_java, &tracking,
                                  &mut data,
                                  near as f32,
                                  far as f32);
        }

        data
    }

    fn synced_frame_data(&self, near: f64, far: f64) -> VRFrameData {
        let mut data = VRFrameData::default();
        if !self.activity_paused && self.is_in_vr_mode() {
            self.fetch_frame_data(self.render_ovr_java.handle(),
                                  &self.predicted_tracking,
                                  &mut data,
                                  near as f32,
                                  far as f32);
        }

        data
    }

    fn reset_pose(&mut self) {
        if !self.activity_paused && self.is_in_vr_mode() {
            unsafe {
                ovr::vrapi_RecenterPose(self.ovr);
            }
        }
    }

    fn sync_poses(&mut self) {
        self.handle_pending_actions();
        if self.activity_paused {
            return;
        }

        if !self.is_in_vr_mode() {
            self.start_present(None);
        }

        self.frame_index += 1;
        self.predicted_display_time =  unsafe { ovr::vrapi_GetPredictedDisplayTime(self.ovr, self.frame_index) };
        self.predicted_tracking = unsafe { ovr::vrapi_GetPredictedTracking2(self.ovr, self.predicted_display_time) };

        if self.gamepads.len() < 2 {
            OculusVRGamepad::refresh_available_gamepads(self.ovr, self.display_id, &mut self.gamepads);
        }

        for gamepad in &self.gamepads {
            gamepad.borrow().set_predicted_display_time(self.predicted_display_time);
        }
    }

    fn bind_framebuffer(&mut self, eye_index: u32) {
        if self.activity_paused || !self.is_in_vr_mode() {
            return;
        }
        let eye = &self.eye_framebuffers[eye_index as usize];
        let swap_chain_index = (self.frame_index % eye.swap_chain_length as i64) as i32;
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, eye.fbos[swap_chain_index as usize]);
        }
    }

    fn get_framebuffers(&self) -> Vec<VRFramebuffer> {
        self.eye_framebuffers.iter().enumerate().map(|(index, fbo)| {
            VRFramebuffer {
                eye_index: index as u32,
                attributes: fbo.attributes,
                viewport: VRViewport::new(0, 0, fbo.width as i32, fbo.height as i32)
            }
        }).collect()
    }

    fn render_layer(&mut self, layer: &VRLayer) {
        if self.activity_paused || !self.is_in_vr_mode() {
            return;
        }

        // Save current fbo to restore it when the frame is submitted.
        let mut current_fbo = 0;
        unsafe {
            gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut current_fbo);
        }

        for (i, eye) in self.eye_framebuffers.iter_mut().enumerate() {
            let swap_chain_index = (self.frame_index % eye.swap_chain_length as i64) as i32;

            if self.read_texture != layer.texture_id {
                // Attach external texture to the used later in BlitFramebuffer.
                unsafe {
                    gl::BindFramebuffer(gl::FRAMEBUFFER, self.read_fbo);
                    gl::FramebufferTexture2D(gl::FRAMEBUFFER,
                                             gl::COLOR_ATTACHMENT0,
                                             gl::TEXTURE_2D,
                                             layer.texture_id, 0);
                }
                self.read_texture = layer.texture_id;
            }

            let texture_size = layer.texture_size.unwrap_or_else(|| {
                (eye.width * 2, eye.height)
            });

            // BlitFramebuffer: external texture to gvr pixel buffer.
            unsafe {
                gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, eye.fbos[swap_chain_index as usize]);
                gl::BindFramebuffer(gl::READ_FRAMEBUFFER, self.read_fbo);
                let w = texture_size.0/2;
                let x0 = (i as u32) * w;
                let x1 = x0 + w;
                gl::BlitFramebuffer(x0 as i32, 0, x1 as i32, texture_size.1 as i32,
                                    0, 0, eye.width as i32, eye.height as i32,
                                    gl::COLOR_BUFFER_BIT, gl::LINEAR);
            }
        }

        // Restore bound fbo
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, current_fbo as u32);
        }
    }

    fn submit_frame(&mut self) {
        if self.activity_paused || !self.is_in_vr_mode() {
            return;
        }

        let eye_projection = self.eye_projection.get();
        let tex_coords = ovr::helpers::ovrMatrix4f_TanAngleMatrixFromProjection(&eye_projection);

        let mut layer = ovr::helpers::vrapi_DefaultLayerProjection2();
        layer.HeadPose = self.predicted_tracking.HeadPose;
        for i in 0..VRAPI_FRAME_LAYER_EYE_MAX as usize {
            let eye = if self.eye_framebuffers.len() > 1 {
                &self.eye_framebuffers[i]
            } else {
                &self.eye_framebuffers[0]
            };

            let swap_chain_index = (self.frame_index % eye.swap_chain_length as i64) as i32;
            layer.Textures[i].ColorSwapChain = eye.swap_chain;
            layer.Textures[i].SwapChainIndex = swap_chain_index;
            layer.Textures[i].TexCoordsFromTanAngles =  tex_coords;
        }

        // Create and insert a new sync object.
        let fence = if OVR_INSERT_FENCE {
            unsafe {
                let display = egl::GetCurrentDisplay();
                let fence = egl::CreateSyncKHR( display, egl::SYNC_FENCE_KHR, ptr::null());
                if fence == egl::NO_SYNC {
                    println!("eglCreateSyncKHR() : EGL_NO_SYNC_KHR");
                } else if egl::ClientWaitSyncKHR(display, fence, egl::SYNC_FLUSH_COMMANDS_BIT as i32, 0 ) == egl::FALSE as i32 {
                    println!("eglClientWaitSyncKHR() : EGL_FALSE" );
                }
                fence as u64
            }
        } else {
            0u64
        };
        
        let mut frame_desc: ovr::ovrSubmitFrameDescription2 = unsafe { mem::zeroed() };
        frame_desc.Flags = 0;
		frame_desc.SwapInterval = 1;
        frame_desc.FrameIndex = self.frame_index as u64;
        frame_desc.DisplayTime = self.predicted_display_time;
        frame_desc.CompletionFence = fence;
        let layers = [&layer.Header as * const ovr::ovrLayerHeader2];
        frame_desc.LayerCount = layers.len() as u32;
        frame_desc.Layers = layers.as_ptr();

        unsafe {
            ovr::vrapi_SubmitFrame2(self.ovr, &frame_desc);
        }
    }

    fn start_present(&mut self, attributes: Option<VRFramebufferAttributes>) {
        if let Some(attributes) = attributes {
            // Overwrite current attributes
            self.attributes = attributes;
        }
        if self.presenting == false {
            // Show the SurfaceView on top of the Android view Hierarchy
            unsafe {
                if let Ok(jni_scope) = JNIScope::attach() {
                    let jni = jni_scope.jni();
                    let env = jni_scope.env;
                    let method = jni_scope.get_method(self.service_java.class, "startPresent", "()V", false);
                    (jni.CallVoidMethod)(env, self.service_java.instance, method);
                }
            }
        }
        if let Err(error) = self.render_ovr_java.attach() {
            error!("Failed to attach to JavaThread {}", error);
            return;
        }

        if self.eye_framebuffers.is_empty() {
            self.create_swap_chains();
            debug_assert!(!self.eye_framebuffers.is_empty());
        }

        self.presenting = true;
        self.enter_vr_mode();
    }

    fn stop_present(&mut self) {
        self.exit_vr_mode();
        if self.presenting == true {
            // Hide the SurfaceView
            unsafe {
                if let Ok(jni_scope) = JNIScope::attach() {
                    let jni = jni_scope.jni();
                    let env = jni_scope.env;
                    let method = jni_scope.get_method(self.service_java.class, "stopPresent", "()V", false);
                    (jni.CallVoidMethod)(env, self.service_java.instance, method);
                }
            }
        }
        self.presenting = false;
    }
}

impl OculusVRDisplay {
    pub fn new(service_java: OVRServiceJava,
               ovr_java: *const ovr::ovrJava)
               -> Arc<RefCell<OculusVRDisplay>> {
        Arc::new(RefCell::new(OculusVRDisplay {
            display_id: utils::new_id(),
            ovr: ptr::null_mut(),
            service_java: service_java,
            data_ovr_java: ovr_java,
            render_ovr_java: OVRJava::default(),
            eye_framebuffers: Vec::new(),
            read_fbo: 0,
            read_texture: 0,
            frame_index: 0,
            predicted_display_time: 0.0,
            predicted_tracking: unsafe { mem::zeroed() },
            eye_projection: Cell::new(ovr::helpers::ovrMatrix4f_CreateIdentity()),
            presenting: false,
            activity_paused: false,
            new_events_hint: false,
            events: Mutex::new(Vec::new()),
            new_pending_action_hint: false,
            pending_action: Mutex::new(None),
            leave_vr_condition: (Mutex::new(false), Condvar::new()),
            gamepads: Vec::new(),
            attributes: Default::default(),
        }))
    }

    fn is_in_vr_mode(&self) -> bool {
        !self.ovr.is_null()
    }

    fn enter_vr_mode(&mut self) {
        if self.is_in_vr_mode() || self.service_java.surface.is_null() {
            return;
        }

        let display = unsafe { egl::GetCurrentDisplay() };

        // Return if display is not ready yet to avoid EGL_NO_DISPLAY error in vrapi_EnterVrMode.
        // Sometines it takes a bit more time for the Display to be ready.
        if display.is_null() {
            return;
        }

        let mut mode = ovr::helpers::vrapi_DefaultModeParms(self.render_ovr_java.handle());
        // Must reset the FLAG_FULLSCREEN window flag when using a SurfaceView
	    mode.Flags |= ovr::ovrModeFlags::VRAPI_MODE_FLAG_RESET_WINDOW_FULLSCREEN as u32;
        mode.Flags |= ovr::ovrModeFlags::VRAPI_MODE_FLAG_NATIVE_WINDOW as u32;

        let env = self.render_ovr_java.jni_scope.as_ref().unwrap().env;
        let surface = self.service_java.surface;

        mode.WindowSurface = unsafe { ANativeWindow_fromSurface(env as *mut _, surface as *mut _) as u64 };
        mode.Display = display as u64;
        mode.ShareContext = unsafe { egl::GetCurrentContext() as u64 };

        debug!("Enter VR Mode {:?}", mode);

        self.ovr = unsafe { ovr::vrapi_EnterVrMode(&mode) };
        if self.ovr.is_null() {
            error!("Entering VR mode failed because the ANativeWindow was not valid.");
            return;
        }

        unsafe {
            ovr::vrapi_SetRemoteEmulation(self.ovr, false);
        }

        // Refresh gamepads after entering VR mode
        OculusVRGamepad::refresh_available_gamepads(self.ovr, self.display_id, &mut self.gamepads);
    }

    fn exit_vr_mode(&mut self) {
        if self.is_in_vr_mode() {
            debug!("Exit VR Mode");
            let ovr = self.ovr;
            self.ovr = ptr::null_mut();

            // Disable gamepads
            for gamepad in &self.gamepads {
                gamepad.borrow_mut().on_exit_vrmode();
            }

            // Exit VR mode
            unsafe {
                ovr::vrapi_LeaveVrMode(ovr);
            }
            //self.render_ovr_java.detach();
        }
    }

    fn is_multiview_supported(&self) -> bool {
        unsafe {
            let extensions = gl::GetString(gl::EXTENSIONS);
            let extensions = str::from_utf8_unchecked(CStr::from_ptr(extensions as *const c_char).to_bytes()).to_string();
            extensions.contains("GL_OVR_multiview") && 
                       ovr::vrapi_GetSystemPropertyInt(&self.render_ovr_java.java, VRAPI_SYS_PROP_MULTIVIEW_AVAILABLE) != 0
        }
    }

    fn create_swap_chains(&mut self) {
        self.eye_framebuffers.clear();

        let recommended_eye_size = Self::recommended_render_size(self.render_ovr_java.handle());

        if self.read_fbo == 0 {
            let mut fbo = 0;
            unsafe {
                gl::GenFramebuffers(1, &mut fbo);
            }
            self.read_fbo = fbo as u32;
        }

        // Save current state
        let mut current_fbo = 0;
        let mut current_texture = 0;
        unsafe {
            gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut current_fbo);
            gl::GetIntegerv(gl::TEXTURE_BINDING_2D, &mut current_texture);
        }

        if self.attributes.multiview && !self.is_multiview_supported(){
            // Ensure that multiview is really supported
            self.attributes.multiview = false;
        }

        let num_buffers = if self.attributes.multiview { 1 }  else { VRAPI_FRAME_LAYER_EYE_MAX as u32 }; 
        for _ in 0..num_buffers {
            let eye_framebuffer = unsafe {
                OculusEyeFramebuffer::new(recommended_eye_size.0, recommended_eye_size.1, &self.attributes)
            };
            self.eye_framebuffers.push(eye_framebuffer);
        }

        // Restore VRGamepadState
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, current_fbo as u32);
            gl::BindFramebuffer(gl::TEXTURE_2D, current_texture as u32);
        }
    }

    fn fetch_capabilities(&self, capabilities: &mut VRDisplayCapabilities) {
        capabilities.can_present = true;
        capabilities.has_orientation = true;
        capabilities.has_external_display = false;
        capabilities.has_position = false;
    }

    fn fetch_eye_parameters(&self, left_eye: &mut VREyeParameters, right_eye: &mut VREyeParameters) {
        let fov_x = unsafe {
            ovr::vrapi_GetSystemPropertyFloat(self.data_ovr_java, VRAPI_SYS_PROP_SUGGESTED_EYE_FOV_DEGREES_X)
        };
        let fov_y = unsafe {
            ovr::vrapi_GetSystemPropertyFloat(self.data_ovr_java, VRAPI_SYS_PROP_SUGGESTED_EYE_FOV_DEGREES_Y)
        };

        left_eye.field_of_view.left_degrees = fov_x as f64;
        left_eye.field_of_view.right_degrees = fov_x as f64;
        left_eye.field_of_view.up_degrees = fov_y as f64;
        left_eye.field_of_view.down_degrees = fov_y as f64;

        right_eye.field_of_view.left_degrees = fov_x as f64;
        right_eye.field_of_view.right_degrees = fov_x as f64;
        right_eye.field_of_view.up_degrees = fov_y as f64;
        right_eye.field_of_view.down_degrees = fov_y as f64;

        let render_size = Self::recommended_render_size(self.data_ovr_java);
        
        left_eye.render_width = render_size.0;
        left_eye.render_height = render_size.1;
        right_eye.render_width = render_size.0;
        right_eye.render_height = render_size.1;
    }

    fn recommended_render_size(java: *const ovr::ovrJava) -> (u32, u32) {
        let w = unsafe {
            ovr::vrapi_GetSystemPropertyInt(java, VRAPI_SYS_PROP_SUGGESTED_EYE_TEXTURE_WIDTH)
        };
        let h = unsafe {
            ovr::vrapi_GetSystemPropertyInt(java, VRAPI_SYS_PROP_SUGGESTED_EYE_TEXTURE_HEIGHT)
        };

        (w as u32, h as u32)
    }

    fn fetch_frame_data(&self,
                        java: *const ovr::ovrJava, 
                        tracking: &ovr::ovrTracking2,
                        out: &mut VRFrameData,
                        near: f32,
                        far: f32) {
        let fov_x = unsafe {
            ovr::vrapi_GetSystemPropertyFloat(java, VRAPI_SYS_PROP_SUGGESTED_EYE_FOV_DEGREES_X)
        };
        let fov_y = unsafe {
            ovr::vrapi_GetSystemPropertyFloat(java, VRAPI_SYS_PROP_SUGGESTED_EYE_FOV_DEGREES_Y)
        };

        // Projection Matrix
        let projection = ovr::helpers::ovrMatrix4f_CreateProjectionFov(fov_x, fov_y, 0.0, 0.0, near, far);
        self.eye_projection.set(projection); // Will be used in submit Frame.
        let projection = ovr_mat4_to_array(&projection);

        out.left_projection_matrix = projection;
        out.right_projection_matrix = projection;

        // View Matrix
        out.left_view_matrix = ovr_mat4_to_array(&tracking.Eye[0].ViewMatrix);
        out.right_view_matrix = ovr_mat4_to_array(&tracking.Eye[1].ViewMatrix);

        // Pose
        out.pose.orientation = Some(ovr_quat_to_array(&tracking.HeadPose.Pose.Orientation));
        out.pose.position = Some(ovr_vec3_to_array(&tracking.HeadPose.Pose.Position));
        out.pose.linear_velocity = Some(ovr_vec3_to_array(&tracking.HeadPose.LinearVelocity));
        out.pose.linear_acceleration = Some(ovr_vec3_to_array(&tracking.HeadPose.LinearAcceleration));
        out.pose.angular_velocity = Some(ovr_vec3_to_array(&tracking.HeadPose.AngularVelocity));
        out.pose.angular_acceleration = Some(ovr_vec3_to_array(&tracking.HeadPose.AngularAcceleration));

        // Timestamp
        out.timestamp = tracking.HeadPose.TimeInSeconds * 1000.0;
    }

    // Warning: this function is called from java Main thread
    // Use mutexes to ensure thread safety and process the event in sync with the render loop.
    pub fn pause(&mut self) {
        let mut left = self.leave_vr_condition.0.lock().unwrap();
        *left = false;
        let wait_until_vr_mode_left = self.presenting;

        {
            let mut pending_action = self.pending_action.lock().unwrap();
            *pending_action = Some(LifeCycleAction::Pause);

            self.new_pending_action_hint = true;
        }

        if wait_until_vr_mode_left {
            // Wait
            while !*left {
                left = self.leave_vr_condition.1.wait(left).unwrap();
            }
        }

        // Trigger Event
        {
            let mut events = self.events.lock().unwrap();
            events.push(VRDisplayEvent::Pause(self.display_id).into());
            self.new_events_hint = true;
        }
    }

    // Warning: this function is called from java Main thread
    // Use mutexes to ensure thread safety and process the event in sync with the render loop.
    pub fn resume(&mut self) {
        {
            let mut pending_action = self.pending_action.lock().unwrap();
            *pending_action = Some(LifeCycleAction::Resume);

            self.new_pending_action_hint = true;
        }
        // Trigger Event
        let mut events = self.events.lock().unwrap();
        events.push(VRDisplayEvent::Resume(self.display_id).into());
        self.new_events_hint = true;
    }

    // Warning: this function is called from java Main thread
    pub fn update_surface(&mut self, surface: ndk::jobject) {
        self.service_java.surface = surface;
        println!("nativeOnUpdate: {:?}", surface);
    }

    fn handle_pending_actions(&mut self) {
        if !self.new_pending_action_hint {
            // Optimization to avoid mutex locks every frame
            // It doesn't matter if events are processed in the next loop iteration
            return;
        }

        let action;
        {
            let mut pending_action = self.pending_action.lock().unwrap();
            action = *pending_action;
            *pending_action = None;
            self.new_pending_action_hint = false;
        };

        match action {
            Some(LifeCycleAction::Resume) => {
                self.activity_paused = false;
                if self.presenting {
                    self.enter_vr_mode();
                }
            },
            Some(LifeCycleAction::Pause) => {
                self.activity_paused = true;
                if self.presenting {
                    self.exit_vr_mode();
                    // Notify condition
                    {
                        let mut left = self.leave_vr_condition.0.lock().unwrap();
                        *left = true;
                        self.leave_vr_condition.1.notify_one();
                    }
                }
            },
            None => {}
        }

        self.new_pending_action_hint = false;
    }

    pub fn poll_events(&mut self, out: &mut Vec<VREvent>) {
        if !self.new_events_hint {
            // Optimization to avoid mutex locks every poll_events call
            // It doesn't matter if events are processed in the next iteration
            return;
        }
        let mut events = self.events.lock().unwrap();
        out.extend(events.drain(..));
        self.new_events_hint = false;
    }

    pub fn fetch_gamepads(&self, out: &mut Vec<OculusVRGamepadPtr>) {
        out.extend(self.gamepads.iter().cloned());
    }
}

struct OculusEyeFramebuffer {
    swap_chain: *mut ovr::ovrTextureSwapChain,
    swap_chain_length: i32,
    fbos: Vec<u32>, // Multiple FBOs for triple buffering,
    depth_buffers: Vec<u32>,
    width: u32,
    height: u32,
    attributes: VRFramebufferAttributes,
}

impl OculusEyeFramebuffer {
    pub unsafe fn new(width: u32, height: u32, attributes: &VRFramebufferAttributes) -> OculusEyeFramebuffer {
        let (texture_type, texture_target) = if attributes.multiview {
            (ovr::ovrTextureType::VRAPI_TEXTURE_TYPE_2D_ARRAY, gl::TEXTURE_2D_ARRAY)
        } else {
            (ovr::ovrTextureType::VRAPI_TEXTURE_TYPE_2D, gl::TEXTURE_2D)
        };

        let swap_chain = ovr::vrapi_CreateTextureSwapChain(texture_type,
                                                           ovr::ovrTextureFormat::VRAPI_TEXTURE_FORMAT_8888,
                                                           width as i32,
                                                           height as i32,
                                                           1,
                                                           true);
        let swap_chain_length = ovr::vrapi_GetTextureSwapChainLength(swap_chain);
        let mut fbos = Vec::new();
        let mut depth_buffers = Vec::new();
        for index in 0..swap_chain_length {
            // Initialize the color buffer texture.
            let texture = ovr::vrapi_GetTextureSwapChainHandle(swap_chain, index);
            gl::BindTexture(texture_target, texture);
            gl::TexParameteri(texture_target, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(texture_target, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(texture_target, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(texture_target, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);


            // Set up the FBO to render to the texture.
            // Optional depth based on attributes.
            let mut fbo = 0;
            let mut depth = 0;

            gl::GenFramebuffers(1, &mut fbo);
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, fbo);
            if attributes.multiview {
                gl::FramebufferTextureMultiviewOVR(gl::DRAW_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, texture, 0, 0, 2);
                if attributes.depth {
                    gl::GenTextures(1, &mut depth);
                    gl::BindTexture(gl::TEXTURE_2D_ARRAY, depth);
                    gl::TexStorage3D(gl::TEXTURE_2D_ARRAY, 1, gl::DEPTH_COMPONENT24, width as i32, height as i32, 2);
                    gl::BindTexture(gl::TEXTURE_2D_ARRAY, 0);
                    gl::FramebufferTextureMultiviewOVR(gl::DRAW_FRAMEBUFFER, gl::DEPTH_ATTACHMENT, texture, 0, 0, 2);
                }

            } else {
                gl::FramebufferTexture2D(gl::DRAW_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, texture, 0);
                if attributes.depth {
                    gl::GenRenderbuffers(1, &mut depth);
                    gl::BindRenderbuffer(gl::RENDERBUFFER, depth);
                    gl::RenderbufferStorage(gl::RENDERBUFFER, gl::DEPTH_COMPONENT24, width as i32, height as i32);
                    gl::BindRenderbuffer(gl::RENDERBUFFER, 0);
                    gl::FramebufferTextureMultiviewOVR(gl::FRAMEBUFFER, gl::DEPTH_ATTACHMENT, texture, 0, 0, 2);
                }
            }

            let status = gl::CheckFramebufferStatus(gl::FRAMEBUFFER);
            if status != gl::FRAMEBUFFER_COMPLETE {
                panic!("Oculus VR Incomplete Framebuffer: {}", status);
            }

            if depth > 0 {
                depth_buffers.push(depth);
            }
            fbos.push(fbo);
        }

        OculusEyeFramebuffer {
            swap_chain,
            swap_chain_length,
            fbos,
            depth_buffers,
            width,
            height,
            attributes: *attributes,
        }
    }
}

impl Drop for OculusEyeFramebuffer {
    fn drop(&mut self) {
        unsafe {
            for fbo in &self.fbos {
                gl::DeleteFramebuffers(1, mem::transmute(fbo));
            }
            for depth_buffer in &self.depth_buffers {
                if self.attributes.multiview {
                    gl::DeleteTextures(1, mem::transmute(depth_buffer));
                } else {
                    gl::DeleteRenderbuffers(1, mem::transmute(depth_buffer));
                }
            }
            ovr::vrapi_DestroyTextureSwapChain(self.swap_chain);
        }
    }
}

#[inline]
fn ovr_mat4_to_array(matrix: &ovr::ovrMatrix4f) -> [f32; 16] {
    [matrix.M[0][0], matrix.M[1][0], matrix.M[2][0], matrix.M[3][0],
     matrix.M[0][1], matrix.M[1][1], matrix.M[2][1], matrix.M[3][1],
     matrix.M[0][2], matrix.M[1][2], matrix.M[2][2], matrix.M[3][2],
     matrix.M[0][3], matrix.M[1][3], matrix.M[2][3], matrix.M[3][3]]
}

#[inline]
pub fn ovr_quat_to_array(q: &ovr::ovrQuatf) -> [f32; 4] {
    [q.x, q.y, q.z, q.w]
}

#[inline]
pub fn ovr_vec3_to_array(v: &ovr::ovrVector3f) -> [f32; 3] {
    [v.x, v.y, v.z]
}
